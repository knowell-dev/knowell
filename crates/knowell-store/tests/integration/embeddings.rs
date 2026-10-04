use knowell_core::{ContentHash, LineRange};
use knowell_store::content::{self, FileChange, NewChunk, NewContent};
use knowell_store::embeddings::*;
use knowell_store::views::{self, GenerationPin};
use knowell_store::{GenerationState, OrganizationId, StoreError};

use crate::common::{fixture, name, path, require_db};

/// Deterministic unit vector (splitmix64; no platform or crate RNG).
fn vector(seed: u64, dims: usize) -> Vec<f32> {
    let mut state = seed;
    let mut v: Vec<f32> = (0..dims)
        .map(|_| {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            ((z >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        })
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

fn h(text: &str) -> ContentHash {
    ContentHash::of(text.as_bytes())
}

fn spec(profile: &str, model: &str, dimensions: u32) -> NewEmbeddingProfile {
    NewEmbeddingProfile {
        name: name(profile),
        provider: "test".into(),
        model: model.into(),
        dimensions,
        input_format_version: "title-text-v1".into(),
    }
}

async fn fill(
    c: &mut sqlx::PgConnection,
    profile: &EmbeddingProfile,
    items: impl Iterator<Item = (ContentHash, Vec<f32>)>,
) -> u64 {
    let rows: Vec<NewEmbedding> = items
        .map(|(prepared_input_hash, vector)| NewEmbedding {
            prepared_input_hash,
            vector,
        })
        .collect();
    upsert_embeddings(c, profile, &rows).await.unwrap()
}

#[tokio::test]
async fn profiles_of_any_dimension_never_mix() {
    let db = require_db!();
    let mut c = db.conn().await;
    let org = knowell_store::hierarchy::create_organization(&mut c, &name("acme"))
        .await
        .unwrap()
        .id;

    let compact = register_profile(&mut c, org, &spec("compact", "m1", 768))
        .await
        .unwrap();
    let extended = register_profile(&mut c, org, &spec("extended", "m1", 3072))
        .await
        .unwrap();
    let twin = register_profile(&mut c, org, &spec("compact-b", "m2", 768))
        .await
        .unwrap();
    // Registration is idempotent; profiles are immutable.
    assert_eq!(
        register_profile(&mut c, org, &spec("compact", "m1", 768))
            .await
            .unwrap(),
        compact
    );
    assert!(matches!(
        register_profile(&mut c, org, &spec("compact", "m1", 1536)).await,
        Err(StoreError::ProfileConflict { .. })
    ));
    assert!(matches!(
        register_profile(&mut c, org, &spec("too-big", "m1", 4001)).await,
        Err(StoreError::InvalidInput(_))
    ));
    assert!(
        sqlx::query("UPDATE embedding_profile SET model = 'other' WHERE id = $1")
            .bind(compact.id)
            .execute(&mut *c)
            .await
            .is_err()
    );
    for p in [&compact, &extended, &twin] {
        assert!(profile_index_ready(&mut c, p).await.unwrap(), "{}", p.name);
    }
    assert_eq!(
        list_profiles(&mut c, org).await.unwrap(),
        vec![compact.clone(), twin.clone(), extended.clone()]
    );

    // Same hashes in two profiles of different sizes; identical vectors in two
    // profiles of the same size under different hashes.
    let n = 30u64;
    assert_eq!(
        fill(
            &mut c,
            &compact,
            (0..n).map(|i| (h(&format!("p{i}")), vector(i, 768)))
        )
        .await,
        n
    );
    assert_eq!(
        fill(
            &mut c,
            &extended,
            (0..n).map(|i| (h(&format!("p{i}")), vector(1000 + i, 3072)))
        )
        .await,
        n
    );
    assert_eq!(
        fill(
            &mut c,
            &twin,
            (0..n).map(|i| (h(&format!("twin{i}")), vector(i, 768)))
        )
        .await,
        n
    );
    // Re-upserting is a no-op.
    assert_eq!(
        fill(
            &mut c,
            &compact,
            (0..n).map(|i| (h(&format!("p{i}")), vector(i, 768)))
        )
        .await,
        0
    );

    let compact_hashes: Vec<ContentHash> = (0..n).map(|i| h(&format!("p{i}"))).collect();
    let twin_hashes: Vec<ContentHash> = (0..n).map(|i| h(&format!("twin{i}"))).collect();

    let hits = nearest(&mut c, &compact, &vector(7, 768), &NearestOptions::new(10))
        .await
        .unwrap();
    assert_eq!(hits.len(), 10);
    assert_eq!(hits[0].prepared_input_hash, h("p7"));
    assert!(hits[0].distance < 0.01, "{}", hits[0].distance);
    assert!(
        hits.iter()
            .all(|x| compact_hashes.contains(&x.prepared_input_hash))
    );
    assert!(hits.windows(2).all(|w| w[0].distance <= w[1].distance));

    let hits = nearest(&mut c, &twin, &vector(7, 768), &NearestOptions::new(10))
        .await
        .unwrap();
    assert_eq!(hits[0].prepared_input_hash, h("twin7"));
    assert!(
        hits.iter()
            .all(|x| twin_hashes.contains(&x.prepared_input_hash))
    );

    let hits = nearest(
        &mut c,
        &extended,
        &vector(1007, 3072),
        &NearestOptions::new(5),
    )
    .await
    .unwrap();
    assert_eq!(hits.len(), 5);
    assert_eq!(hits[0].prepared_input_hash, h("p7"));
    assert!(hits[0].distance < 0.01);

    // A query of the wrong size is rejected, never compared.
    assert!(matches!(
        nearest(&mut c, &extended, &vector(7, 768), &NearestOptions::new(5)).await,
        Err(StoreError::DimensionMismatch {
            expected: 3072,
            actual: 768,
            ..
        })
    ));
    assert!(matches!(
        upsert_embeddings(
            &mut c,
            &extended,
            &[NewEmbedding {
                prepared_input_hash: h("x"),
                vector: vector(1, 768)
            }]
        )
        .await,
        Err(StoreError::DimensionMismatch { .. })
    ));
    let mut bad = vector(1, 768);
    bad[3] = f32::NAN;
    assert!(matches!(
        upsert_embeddings(
            &mut c,
            &compact,
            &[NewEmbedding {
                prepared_input_hash: h("x"),
                vector: bad
            }]
        )
        .await,
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        nearest(&mut c, &compact, &vector(7, 768), &NearestOptions::new(0)).await,
        Err(StoreError::InvalidInput(_))
    ));

    // Stored values are half precision.
    let stored = get_embedding(&mut c, extended.id, &h("p3"))
        .await
        .unwrap()
        .unwrap();
    let original = vector(1003, 3072);
    assert_eq!(stored.len(), 3072);
    assert!(
        stored
            .iter()
            .zip(&original)
            .all(|(a, b)| (a - b).abs() < 1e-3)
    );
    assert_eq!(
        get_embedding(&mut c, twin.id, &h("p3")).await.unwrap(),
        None
    );

    let missing = missing_embeddings(&mut c, compact.id, &[h("p1"), h("p999"), h("twin1")])
        .await
        .unwrap();
    assert_eq!(missing, vec![h("p999"), h("twin1")]);

    // The query matches each profile's own partial index (expression and
    // predicate). With 30 rows a full sort is cheaper than the index, so the
    // sort-based alternatives are switched off; with thousands of rows per
    // profile the planner picks the index on its own.
    for setting in ["enable_seqscan", "enable_bitmapscan", "enable_sort"] {
        sqlx::query(sqlx::AssertSqlSafe(format!("SET {setting} = off")))
            .execute(&mut *c)
            .await
            .unwrap();
    }
    for (p, q) in [(&compact, vector(1, 768)), (&extended, vector(1, 3072))] {
        let plan = explain_nearest(&mut c, p, &q, &NearestOptions::new(5))
            .await
            .unwrap();
        assert!(plan.contains(&p.index_name()), "{plan}");
        let other = if p.id == compact.id {
            &extended
        } else {
            &compact
        };
        assert!(!plan.contains(&other.index_name()), "{plan}");
    }
    sqlx::query("RESET ALL").execute(&mut *c).await.unwrap();
}

#[tokio::test]
async fn a_missing_index_is_reported_and_repaired() {
    let db = require_db!();
    let mut c = db.conn().await;
    let org = knowell_store::hierarchy::create_organization(&mut c, &name("acme"))
        .await
        .unwrap()
        .id;
    let p = register_profile(&mut c, org, &spec("compact", "m1", 8))
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP INDEX {}",
        p.index_name()
    )))
    .execute(&mut *c)
    .await
    .unwrap();
    assert!(matches!(
        nearest(&mut c, &p, &vector(1, 8), &NearestOptions::new(1)).await,
        Err(StoreError::ProfileIndexMissing { .. })
    ));
    register_profile(&mut c, org, &spec("compact", "m1", 8))
        .await
        .unwrap();
    assert!(
        nearest(&mut c, &p, &vector(1, 8), &NearestOptions::new(1))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn concurrent_registration_builds_one_index() {
    let db = require_db!();
    let mut c = db.conn().await;
    let org = knowell_store::hierarchy::create_organization(&mut c, &name("acme"))
        .await
        .unwrap()
        .id;
    let mut tasks = Vec::new();
    let ready = std::sync::Arc::new(tokio::sync::Barrier::new(4));
    for _ in 0..4 {
        let store = db.store.clone();
        let ready = ready.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = store.acquire().await.unwrap();
            ready.wait().await;
            register_profile(&mut c, org, &spec("compact", "m1", 16))
                .await
                .unwrap()
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.push(t.await.unwrap().id);
    }
    ids.dedup();
    assert_eq!(ids.len(), 1);
    let indexes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_indexes WHERE tablename = 'embedding' AND indexname LIKE 'embedding_hnsw_%'",
    )
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(indexes, 1);
    // A different name must still report a settings conflict, never silently
    // return the profile registered under the original name.
    assert!(matches!(
        register_profile(&mut c, org, &spec("other-name", "m1", 16)).await,
        Err(StoreError::AlreadyExists { .. })
    ));
    assert_eq!(list_profiles(&mut c, org).await.unwrap().len(), 1);
}

async fn index_files(
    c: &mut sqlx::PgConnection,
    org: OrganizationId,
    view: knowell_store::ViewId,
    profile: &EmbeddingProfile,
    count: u64,
    in_view: impl Fn(u64) -> bool,
) -> i64 {
    let contents: Vec<NewContent> = (0..count)
        .map(|i| NewContent {
            hash: h(&format!("file{i}")),
            size_bytes: 10,
            language: Some("rust".into()),
            redacted_text: Some(format!("fn f{i}() {{}}")),
        })
        .collect();
    content::upsert_contents(c, org, &contents).await.unwrap();
    let chunks: Vec<NewChunk> = (0..count)
        .map(|i| NewChunk {
            content_hash: h(&format!("file{i}")),
            parser_version: "p1".into(),
            ordinal: 0,
            lines: LineRange::new(1, 1).unwrap(),
            start_byte: 0,
            end_byte: 10,
            kind: "function".into(),
            symbol_path: None,
            prepared_input_hash: h(&format!("prepared{i}")),
        })
        .collect();
    content::upsert_chunks(c, org, &chunks).await.unwrap();
    fill(
        c,
        profile,
        (0..count).map(|i| (h(&format!("prepared{i}")), vector(i, 768))),
    )
    .await;
    let g = views::begin_generation(c, view, None).await.unwrap();
    let changes: Vec<FileChange> = (0..count)
        .filter(|i| in_view(*i))
        .map(|i| FileChange::Upsert {
            path: path(&format!("src/f{i}.rs")),
            content_hash: h(&format!("file{i}")),
            renamed_from: None,
        })
        .collect();
    content::apply_file_changes(c, view, g, &changes)
        .await
        .unwrap();
    views::activate_generation(c, view, g).await.unwrap();
    g
}

#[tokio::test]
async fn scoped_search_fills_k_from_pinned_files_only() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let profile = register_profile(&mut c, fx.org.id, &spec("compact", "m1", 768))
        .await
        .unwrap();
    // 200 inputs; only every fourth one is a chunk of a file in the view.
    let g = index_files(&mut c, fx.org.id, fx.view.id, &profile, 200, |i| i % 4 == 0).await;
    let scope = vec![GenerationPin {
        view: fx.view.id,
        generation: g,
    }];
    let in_scope =
        |hash: &ContentHash| (0..200u64).any(|i| i % 4 == 0 && *hash == h(&format!("prepared{i}")));

    // Unscoped, the out-of-view input itself is the best hit.
    let hits = nearest(&mut c, &profile, &vector(1, 768), &NearestOptions::new(5))
        .await
        .unwrap();
    assert_eq!(hits[0].prepared_input_hash, h("prepared1"));

    // Scoped, k is still filled, only with inputs from pinned files.
    let options = NearestOptions {
        k: 5,
        ef_search: Some(10),
        scope: Some(scope.clone()),
        ..NearestOptions::new(5)
    };
    let hits = nearest(&mut c, &profile, &vector(1, 768), &options)
        .await
        .unwrap();
    assert_eq!(hits.len(), 5);
    assert!(hits.iter().all(|x| in_scope(&x.prepared_input_hash)));
    let hits = nearest(&mut c, &profile, &vector(8, 768), &options)
        .await
        .unwrap();
    assert_eq!(hits[0].prepared_input_hash, h("prepared8"));

    // Hits resolve to files in the pinned view.
    let located = content::locate_prepared_inputs(&mut c, fx.org.id, &scope, &[h("prepared8")])
        .await
        .unwrap();
    assert_eq!(located.len(), 1);
    assert_eq!(located[0].path, path("src/f8.rs"));

    // Pins of another tenant's view match nothing here.
    let other = fixture(&mut c, "web").await;
    let og = views::begin_generation(&mut c, other.view.id, None)
        .await
        .unwrap();
    content::apply_file_changes(
        &mut c,
        other.view.id,
        og,
        &[FileChange::Upsert {
            path: path("leak.rs"),
            content_hash: h("file1"),
            renamed_from: None,
        }],
    )
    .await
    .unwrap();
    views::activate_generation(&mut c, other.view.id, og)
        .await
        .unwrap();
    let foreign = NearestOptions {
        scope: Some(vec![GenerationPin {
            view: other.view.id,
            generation: og,
        }]),
        ..NearestOptions::new(5)
    };
    assert!(
        nearest(&mut c, &profile, &vector(1, 768), &foreign)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn index_generations_are_fenced_per_view_and_profile() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;
    let profile = register_profile(&mut c, fx.org.id, &spec("compact", "m1", 4))
        .await
        .unwrap();

    let g1 = views::begin_generation(&mut c, view, None).await.unwrap();
    views::activate_generation(&mut c, view, g1).await.unwrap();
    let pin1 = GenerationPin {
        view,
        generation: g1,
    };
    let i1 = begin_index_generation(&mut c, pin1, profile.id)
        .await
        .unwrap();
    assert_eq!(i1.state, GenerationState::Building);
    assert_eq!(
        begin_index_generation(&mut c, pin1, profile.id)
            .await
            .unwrap(),
        i1
    );
    update_index_counts(&mut c, i1.id, 10, 7).await.unwrap();
    let a1 = activate_index_generation(&mut c, i1.id).await.unwrap();
    assert_eq!(
        (a1.state, a1.chunk_count, a1.embedded_count),
        (GenerationState::Active, 10, 7)
    );

    let g2 = views::begin_generation(&mut c, view, None).await.unwrap();
    let i2 = begin_index_generation(
        &mut c,
        GenerationPin {
            view,
            generation: g2,
        },
        profile.id,
    )
    .await
    .unwrap();
    activate_index_generation(&mut c, i2.id).await.unwrap();
    let active = active_index_generation(&mut c, view, profile.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.id, i2.id);

    // The older one cannot come back.
    assert!(matches!(
        activate_index_generation(&mut c, i1.id).await,
        Err(StoreError::StaleGeneration { .. })
    ));
    assert!(matches!(
        update_index_counts(&mut c, i1.id, 1, 1).await,
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        fail_index_generation(&mut c, i1.id, "late").await,
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        begin_index_generation(
            &mut c,
            GenerationPin {
                view,
                generation: 99
            },
            profile.id
        )
        .await,
        Err(StoreError::NotFound { .. })
    ));
}

#[tokio::test]
async fn per_path_inputs_scope_search_and_count_coverage() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;
    let org = fx.org.id;
    let profile = register_profile(&mut c, org, &spec("compact", "m1", 8))
        .await
        .unwrap();
    let parser = "p1";
    // One content at two paths; each path has its own prepared input.
    content::upsert_contents(
        &mut c,
        org,
        &[NewContent {
            hash: h("body"),
            size_bytes: 4,
            language: None,
            redacted_text: Some("body".into()),
        }],
    )
    .await
    .unwrap();
    content::upsert_chunks(
        &mut c,
        org,
        &[NewChunk {
            content_hash: h("body"),
            parser_version: parser.into(),
            ordinal: 0,
            lines: LineRange::new(1, 1).unwrap(),
            start_byte: 0,
            end_byte: 4,
            kind: "text".into(),
            symbol_path: None,
            // The content-level row records the first path's input.
            prepared_input_hash: h("a.rs:body"),
        }],
    )
    .await
    .unwrap();
    let g1 = views::begin_generation(&mut c, view, None).await.unwrap();
    let pin = GenerationPin {
        view,
        generation: g1,
    };
    content::apply_file_changes(
        &mut c,
        view,
        g1,
        &[
            FileChange::Upsert {
                path: path("a.rs"),
                content_hash: h("body"),
                renamed_from: None,
            },
            FileChange::Upsert {
                path: path("b.rs"),
                content_hash: h("body"),
                renamed_from: None,
            },
        ],
    )
    .await
    .unwrap();
    let inputs: Vec<content::NewChunkInput> = ["a.rs", "b.rs"]
        .iter()
        .map(|p| content::NewChunkInput {
            path: path(p),
            content_hash: h("body"),
            ordinal: 0,
            prepared_input_hash: h(&format!("{p}:body")),
            embed: true,
        })
        .collect();
    content::replace_chunk_inputs(&mut c, pin, parser, &[path("a.rs"), path("b.rs")], &inputs)
        .await
        .unwrap();
    assert!(
        index_generation_at(&mut c, pin, profile.id)
            .await
            .unwrap()
            .is_none()
    );
    let started = begin_index_generation(&mut c, pin, profile.id)
        .await
        .unwrap();
    assert_eq!(
        index_generation_at(&mut c, pin, profile.id).await.unwrap(),
        Some(started)
    );
    views::activate_generation(&mut c, view, g1).await.unwrap();

    let coverage = input_coverage(&mut c, pin, profile.id, parser)
        .await
        .unwrap();
    assert_eq!(
        coverage,
        InputCoverage {
            inputs: 2,
            embedded: 0
        }
    );
    assert!(!coverage.is_complete());
    // Only b.rs's input is embedded: coverage is partial, and b.rs's vector
    // (whose hash the content-level chunk row does not know) is in scope.
    fill(
        &mut c,
        &profile,
        [(h("b.rs:body"), vector(3, 8))].into_iter(),
    )
    .await;
    let coverage = input_coverage(&mut c, pin, profile.id, parser)
        .await
        .unwrap();
    assert_eq!((coverage.inputs, coverage.embedded), (2, 1));
    // A vector of an input no file of the view records is out of scope.
    fill(&mut c, &profile, [(h("stray"), vector(3, 8))].into_iter()).await;
    let options = NearestOptions {
        scope: Some(vec![pin]),
        ..NearestOptions::new(5)
    };
    let hits = nearest(&mut c, &profile, &vector(3, 8), &options)
        .await
        .unwrap();
    let hashes: Vec<ContentHash> = hits.iter().map(|n| n.prepared_input_hash).collect();
    assert_eq!(hashes, vec![h("b.rs:body")]);
    let located = content::locate_chunk_inputs(&mut c, org, &[pin], &hashes)
        .await
        .unwrap();
    assert_eq!(located.len(), 1);
    assert_eq!(located[0].path, path("b.rs"));
    fill(
        &mut c,
        &profile,
        [(h("a.rs:body"), vector(4, 8))].into_iter(),
    )
    .await;
    assert!(
        input_coverage(&mut c, pin, profile.id, parser)
            .await
            .unwrap()
            .is_complete()
    );
    // Inputs that are not meant to be embedded are not counted.
    content::replace_chunk_inputs(
        &mut c,
        pin,
        parser,
        &[path("a.rs")],
        &[content::NewChunkInput {
            embed: false,
            prepared_input_hash: h("a.rs:other"),
            ..inputs[0].clone()
        }],
    )
    .await
    .unwrap();
    let coverage = input_coverage(&mut c, pin, profile.id, parser)
        .await
        .unwrap();
    assert_eq!((coverage.inputs, coverage.embedded), (1, 1));
    assert_eq!(
        input_coverage(&mut c, pin, profile.id, "p2").await.unwrap(),
        InputCoverage::default()
    );
}

#[tokio::test]
async fn scoped_ann_filters_paths_and_languages_before_limit_with_literal_prefixes() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "filtered-api").await;
    let other = fixture(&mut c, "foreign-api").await;
    let profile = register_profile(&mut c, fx.org.id, &spec("filtered", "m1", 8))
        .await
        .unwrap();
    let query = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let aligned = |cosine: f32| {
        vec![
            cosine,
            (1.0 - cosine * cosine).sqrt(),
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
        ]
    };
    // All closer distractors must be eliminated inside ANN, before k=2.
    // '%' and '_' are literal path characters, not SQL LIKE wildcards.
    let rows: Vec<(&str, Option<&str>, f32)> = vec![
        ("src/%literal_/reader.rs", Some("rust"), 0.9),
        ("src/%literal_/parser.rs", Some("rust"), 0.8),
        ("src/XliteralY/closest.rs", Some("rust"), 1.0),
        ("src/%literal_/script.py", Some("python"), 1.0),
        ("vendor/closest.rs", Some("rust"), 1.0),
        ("src/%literal_/deleted.rs", Some("rust"), 1.0),
        ("src/%literal_/unknown.bin", None, 0.7),
    ];
    let blobs: Vec<_> = rows
        .iter()
        .enumerate()
        .map(|(i, (_, language, _))| {
            let body = format!("synthetic filtered body {i}");
            NewContent {
                hash: h(&body),
                size_bytes: body.len() as u64,
                language: language.map(str::to_owned),
                // The unknown .bin models unavailable binary text. A missing
                // caller hint alone cannot override canonical source detection.
                redacted_text: if language.is_none() { None } else { Some(body) },
            }
        })
        .collect();
    content::upsert_contents(&mut c, fx.org.id, &blobs)
        .await
        .unwrap();
    let chunks: Vec<_> = blobs
        .iter()
        .enumerate()
        .map(|(i, blob)| NewChunk {
            content_hash: blob.hash,
            parser_version: "filter-parser-1".into(),
            ordinal: 0,
            lines: LineRange::new(1, 1).unwrap(),
            start_byte: 0,
            end_byte: blob.size_bytes,
            kind: "text".into(),
            symbol_path: None,
            prepared_input_hash: h(&format!("legacy filtered input {i}")),
        })
        .collect();
    content::upsert_chunks(&mut c, fx.org.id, &chunks)
        .await
        .unwrap();
    fill(
        &mut c,
        &profile,
        rows.iter().enumerate().map(|(i, (_, _, similarity))| {
            // This legacy input is shadowed by parser.rs's per-path input.
            let similarity = if i == 1 { 1.0 } else { *similarity };
            (chunks[i].prepared_input_hash, aligned(similarity))
        }),
    )
    .await;
    let parser_input = h("per-path parser input");
    fill(&mut c, &profile, [(parser_input, aligned(0.8))].into_iter()).await;
    let g1 = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    let pin1 = GenerationPin {
        view: fx.view.id,
        generation: g1,
    };
    let changes: Vec<_> = rows
        .iter()
        .enumerate()
        .map(|(i, (p, _, _))| FileChange::Upsert {
            path: path(p),
            content_hash: blobs[i].hash,
            renamed_from: None,
        })
        .collect();
    content::apply_file_changes(&mut c, fx.view.id, g1, &changes)
        .await
        .unwrap();
    content::replace_chunk_inputs(
        &mut c,
        pin1,
        "filter-parser-1",
        &[path(rows[1].0)],
        &[content::NewChunkInput {
            path: path(rows[1].0),
            content_hash: blobs[1].hash,
            ordinal: 0,
            prepared_input_hash: parser_input,
            embed: true,
        }],
    )
    .await
    .unwrap();
    views::activate_generation(&mut c, fx.view.id, g1)
        .await
        .unwrap();
    let g2 = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    content::apply_file_changes(
        &mut c,
        fx.view.id,
        g2,
        &[FileChange::Delete {
            path: path(rows[5].0),
        }],
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
    let options = NearestOptions {
        scope: Some(vec![pin2]),
        path_prefixes: vec!["src/%literal_/".into()],
        languages: vec!["rust".into()],
        ..NearestOptions::new(2)
    };
    let hits = nearest(&mut c, &profile, &query, &options).await.unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| hit.prepared_input_hash)
            .collect::<Vec<_>>(),
        vec![chunks[0].prepared_input_hash, parser_input]
    );
    let located = content::locate_chunk_inputs(
        &mut c,
        fx.org.id,
        &[pin2],
        &hits
            .iter()
            .map(|hit| hit.prepared_input_hash)
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    assert_eq!(located.len(), 2);
    assert!(
        located
            .iter()
            .all(|item| item.path.as_str().starts_with("src/%literal_/"))
    );

    // The same request pinned before deletion still sees its closest old input.
    let old = nearest(
        &mut c,
        &profile,
        &query,
        &NearestOptions {
            scope: Some(vec![pin1]),
            ..options.clone()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        old.iter()
            .map(|hit| hit.prepared_input_hash)
            .collect::<Vec<_>>(),
        vec![chunks[5].prepared_input_hash, chunks[0].prepared_input_hash]
    );
    let case_mismatch = NearestOptions {
        path_prefixes: vec!["SRC/%literal_/".into()],
        ..options.clone()
    };
    assert!(
        nearest(&mut c, &profile, &query, &case_mismatch)
            .await
            .unwrap()
            .is_empty()
    );
    let python = NearestOptions {
        languages: vec!["python".into()],
        ..options.clone()
    };
    let hits = nearest(&mut c, &profile, &query, &python).await.unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| hit.prepared_input_hash)
            .collect::<Vec<_>>(),
        vec![chunks[3].prepared_input_hash]
    );
    let unknown_language = NearestOptions {
        languages: vec!["text".into()],
        ..options.clone()
    };
    let hits = nearest(&mut c, &profile, &query, &unknown_language)
        .await
        .unwrap();
    // Catalog presentation calls an unknown language "text", but explicit
    // retrieval language filters admit only a known stored language.
    assert!(hits.is_empty());
    let unknown_unrestricted = NearestOptions {
        languages: Vec::new(),
        path_prefixes: vec![rows[6].0.into()],
        ..options.clone()
    };
    let hits = nearest(&mut c, &profile, &query, &unknown_unrestricted)
        .await
        .unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| hit.prepared_input_hash)
            .collect::<Vec<_>>(),
        vec![chunks[6].prepared_input_hash]
    );

    let foreign_generation = views::begin_generation(&mut c, other.view.id, None)
        .await
        .unwrap();
    content::upsert_contents(&mut c, other.org.id, &[blobs[0].clone()])
        .await
        .unwrap();
    content::upsert_chunks(&mut c, other.org.id, &[chunks[0].clone()])
        .await
        .unwrap();
    content::apply_file_changes(
        &mut c,
        other.view.id,
        foreign_generation,
        &[FileChange::Upsert {
            path: path("src/%literal_/foreign.rs"),
            content_hash: blobs[0].hash,
            renamed_from: None,
        }],
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
    let foreign = NearestOptions {
        scope: Some(vec![foreign_pin]),
        ..options.clone()
    };
    assert!(
        nearest(&mut c, &profile, &query, &foreign)
            .await
            .unwrap()
            .is_empty()
    );
    let mixed = NearestOptions {
        scope: Some(vec![foreign_pin, pin2]),
        ..options.clone()
    };
    assert_eq!(
        nearest(&mut c, &profile, &query, &mixed).await.unwrap(),
        nearest(&mut c, &profile, &query, &options).await.unwrap()
    );
    let unscoped = NearestOptions {
        scope: None,
        ..options
    };
    assert!(matches!(
        nearest(&mut c, &profile, &query, &unscoped).await,
        Err(StoreError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn scoped_ann_uses_occurrence_languages_for_per_path_inputs_before_limit() {
    assert_occurrence_languages_before_limit(true).await;
}

#[tokio::test]
async fn scoped_ann_uses_occurrence_languages_for_legacy_shared_inputs_before_limit() {
    assert_occurrence_languages_before_limit(false).await;
}

async fn assert_occurrence_languages_before_limit(per_path: bool) {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "occurrence-languages").await;
    let foreign = fixture(&mut c, "foreign-occurrence-languages").await;
    let profile = register_profile(&mut c, fx.org.id, &spec("occurrence-language", "m1", 8))
        .await
        .unwrap();
    let parser = "occurrence-language-parser-1";
    let shared = "pub fn occurrence_language_probe() {}\n";
    let neutral = "synthetic source without a language clue\n";
    let unavailable = "synthetic binary blob with no stored source text";
    let bodies = [
        shared,
        "closest synthetic Markdown distractor\n",
        "another synthetic Markdown distractor\n",
        neutral,
        "pub fn outside_the_literal_prefix() {}\n",
        unavailable,
    ];
    let blobs: Vec<_> = bodies
        .iter()
        .enumerate()
        .map(|(ordinal, body)| NewContent {
            hash: h(body),
            size_bytes: body.len() as u64,
            // The shared blob's first hint cannot classify both occurrences.
            language: Some("markdown".into()),
            // Only the separate binary blob lacks source. Available unknown
            // extensions classify as text, consistently with the parser.
            redacted_text: (ordinal != 5).then(|| (*body).into()),
        })
        .collect();
    content::upsert_contents(&mut c, fx.org.id, &blobs)
        .await
        .unwrap();
    let chunks: Vec<_> = blobs
        .iter()
        .enumerate()
        .map(|(ordinal, blob)| NewChunk {
            content_hash: blob.hash,
            parser_version: parser.into(),
            ordinal: 0,
            lines: LineRange::new(1, 1).unwrap(),
            start_byte: 0,
            end_byte: blob.size_bytes,
            kind: "text".into(),
            symbol_path: None,
            prepared_input_hash: h(&format!("legacy occurrence language input {ordinal}")),
        })
        .collect();
    content::upsert_chunks(&mut c, fx.org.id, &chunks)
        .await
        .unwrap();
    let rows = [
        ("src/%literal_/shared.md", h(shared), 0.95_f32),
        ("src/%literal_/desired.rs", h(shared), 0.8),
        ("src/%literal_/closest.md", h(bodies[1]), 1.0),
        ("src/%literal_/next.md", h(bodies[2]), 0.99),
        ("src/%literal_/unknown.bin", h(unavailable), 0.7),
        ("src/%literal_/explicit.txt", h(neutral), 0.7),
        ("outside/closest.rs", h(bodies[4]), 1.0),
        ("src/%literal_/ordinary.unknown", h(neutral), 0.7),
    ];
    let input_for = |file: &str, hash: ContentHash| {
        if per_path {
            h(&format!("per-path occurrence language input {file}"))
        } else {
            chunks
                .iter()
                .find(|chunk| chunk.content_hash == hash)
                .unwrap()
                .prepared_input_hash
        }
    };
    let generation = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    let pin = GenerationPin {
        view: fx.view.id,
        generation,
    };
    let changes: Vec<_> = rows
        .iter()
        .map(|(file, hash, _)| FileChange::Upsert {
            path: path(file),
            content_hash: *hash,
            renamed_from: None,
        })
        .collect();
    content::apply_file_changes(&mut c, fx.view.id, generation, &changes)
        .await
        .unwrap();
    if per_path {
        let paths: Vec<_> = rows.iter().map(|(file, _, _)| path(file)).collect();
        let inputs: Vec<_> = rows
            .iter()
            .map(|(file, hash, _)| content::NewChunkInput {
                path: path(file),
                content_hash: *hash,
                ordinal: 0,
                prepared_input_hash: input_for(file, *hash),
                embed: true,
            })
            .collect();
        content::replace_chunk_inputs(&mut c, pin, parser, &paths, &inputs)
            .await
            .unwrap();
    }
    views::activate_generation(&mut c, fx.view.id, generation)
        .await
        .unwrap();
    let files = content::files_at(&mut c, pin).await.unwrap();
    let language_at = |file: &str| {
        files
            .iter()
            .find(|version| version.path.as_str() == file)
            .unwrap()
            .language
            .as_deref()
    };
    assert_eq!(language_at(rows[0].0), Some("markdown"));
    assert_eq!(language_at(rows[1].0), Some("rust"));
    assert_eq!(language_at(rows[4].0), None);
    assert_eq!(language_at(rows[5].0), Some("text"));
    assert_eq!(language_at(rows[7].0), Some("text"));
    let aligned = |cosine: f32| {
        vec![
            cosine,
            (1.0 - cosine * cosine).sqrt(),
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
        ]
    };
    let query = aligned(1.0);
    let legacy_similarities = [if per_path { 1.0 } else { 0.8 }, 1.0, 0.99, 0.7, 1.0, 0.7];
    fill(
        &mut c,
        &profile,
        chunks
            .iter()
            .zip(legacy_similarities)
            .map(|(chunk, similarity)| (chunk.prepared_input_hash, aligned(similarity))),
    )
    .await;
    if per_path {
        fill(
            &mut c,
            &profile,
            rows.iter()
                .map(|(file, hash, similarity)| (input_for(file, *hash), aligned(*similarity))),
        )
        .await;
    }
    let options = NearestOptions {
        scope: Some(vec![pin]),
        path_prefixes: vec!["src/%literal_/".into()],
        languages: vec!["rust".into()],
        ..NearestOptions::new(1)
    };
    let expected = input_for(rows[1].0, rows[1].1);
    let unfiltered = NearestOptions {
        languages: Vec::new(),
        ..options.clone()
    };
    let closer = nearest(&mut c, &profile, &query, &unfiltered)
        .await
        .unwrap();
    assert_eq!(closer.len(), 1);
    assert_eq!(
        closer[0].prepared_input_hash,
        input_for(rows[2].0, rows[2].1)
    );
    let hits = nearest(&mut c, &profile, &query, &options).await.unwrap();
    assert_eq!(hits.len(), 1, "a filtered Rust occurrence must survive k=1");
    assert_eq!(hits[0].prepared_input_hash, expected);
    let located = content::locate_chunk_inputs(&mut c, fx.org.id, &[pin], &[expected])
        .await
        .unwrap();
    let located_paths: Vec<_> = located.iter().map(|item| item.path.as_str()).collect();
    if per_path {
        // The closer content-level vector is shadowed by both per-path inputs.
        assert_ne!(expected, chunks[0].prepared_input_hash);
        assert_eq!(located_paths, vec![rows[1].0]);
    } else {
        // ANN admits an input if one occurrence qualifies. Localization still
        // returns both bindings; the engine applies the language filter again.
        assert_eq!(located_paths, vec![rows[1].0, rows[0].0]);
    }
    for (file, hash, language, admitted) in [
        (rows[0].0, rows[0].1, "rust", false),
        (rows[4].0, rows[4].1, "text", false),
        (rows[5].0, rows[5].1, "text", true),
        (rows[7].0, rows[7].1, "text", true),
    ] {
        let filtered = NearestOptions {
            path_prefixes: vec![file.into()],
            languages: vec![language.into()],
            ..options.clone()
        };
        let result = nearest(&mut c, &profile, &query, &filtered).await.unwrap();
        assert_eq!(
            result.len(),
            if admitted { 1 } else { 0 },
            "{file}: {language}"
        );
        if admitted {
            assert_eq!(result[0].prepared_input_hash, input_for(file, hash));
        }
    }

    content::upsert_contents(&mut c, foreign.org.id, &[blobs[0].clone()])
        .await
        .unwrap();
    content::upsert_chunks(&mut c, foreign.org.id, &[chunks[0].clone()])
        .await
        .unwrap();
    let foreign_generation = views::begin_generation(&mut c, foreign.view.id, None)
        .await
        .unwrap();
    let foreign_pin = GenerationPin {
        view: foreign.view.id,
        generation: foreign_generation,
    };
    let foreign_path = path("src/%literal_/foreign.rs");
    content::apply_file_changes(
        &mut c,
        foreign.view.id,
        foreign_generation,
        &[FileChange::Upsert {
            path: foreign_path.clone(),
            content_hash: h(shared),
            renamed_from: None,
        }],
    )
    .await
    .unwrap();
    if per_path {
        content::replace_chunk_inputs(
            &mut c,
            foreign_pin,
            parser,
            std::slice::from_ref(&foreign_path),
            &[content::NewChunkInput {
                path: foreign_path.clone(),
                content_hash: h(shared),
                ordinal: 0,
                prepared_input_hash: expected,
                embed: true,
            }],
        )
        .await
        .unwrap();
    }
    views::activate_generation(&mut c, foreign.view.id, foreign_generation)
        .await
        .unwrap();
    let only_foreign = NearestOptions {
        scope: Some(vec![foreign_pin]),
        ..options.clone()
    };
    assert!(
        nearest(&mut c, &profile, &query, &only_foreign)
            .await
            .unwrap()
            .is_empty()
    );
    let mixed = NearestOptions {
        scope: Some(vec![foreign_pin, pin]),
        ..options.clone()
    };
    assert_eq!(
        nearest(&mut c, &profile, &query, &mixed).await.unwrap(),
        hits
    );
    let next_generation = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    content::apply_file_changes(
        &mut c,
        fx.view.id,
        next_generation,
        &[FileChange::Delete {
            path: path(rows[1].0),
        }],
    )
    .await
    .unwrap();
    views::activate_generation(&mut c, fx.view.id, next_generation)
        .await
        .unwrap();
    let latest = NearestOptions {
        scope: Some(vec![
            GenerationPin {
                view: fx.view.id,
                generation: next_generation,
            },
            foreign_pin,
        ]),
        ..options.clone()
    };
    assert!(
        nearest(&mut c, &profile, &query, &latest)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        nearest(&mut c, &profile, &query, &options).await.unwrap(),
        hits
    );
}
