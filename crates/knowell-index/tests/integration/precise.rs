//! Durable compiler imports, activation fences and non-inheritance.

use std::collections::BTreeMap;
use std::sync::Arc;

use knowell_core::ContentHash;
use knowell_index::{PreparedScipImport, Priority, ScipImportLimits, ScipImportManifest};
use knowell_store::graph;
use knowell_store::{EvidenceType, GenerationState};
use protobuf::{EnumOrUnknown, Message, MessageField};
use scip::types as pb;

use crate::common::{
    CountingEmbedder, TestDb, active_pin, config, fixture_workspace, git_available, indexer_with,
    path, require_db, view_of,
};

const PROJECT: &str = "billing-api";
const SOURCE_PATH: &str = "src/precise.rs";
const SOURCE: &str = "fn target() {}\nfn caller() { target(); }\n";
const TARGET: &str = "synthetic-scip cargo fixture 1 precise/target().";
const CALLER: &str = "synthetic-scip cargo fixture 1 precise/caller().";

fn artifact(source: &str, revision: String) -> PreparedScipImport {
    let occurrence = |row: usize, name: &str, moniker: &str, definition: bool, last: bool| {
        let line = source.lines().nth(row).unwrap();
        let column = if last {
            line.rfind(name)
        } else {
            line.find(name)
        }
        .unwrap();
        pb::Occurrence {
            range: vec![row as i32, column as i32, (column + name.len()) as i32],
            symbol: moniker.to_owned(),
            symbol_roles: i32::from(definition),
            ..Default::default()
        }
    };
    let message = pb::Index {
        metadata: MessageField::some(pb::Metadata {
            tool_info: MessageField::some(pb::ToolInfo {
                name: "synthetic-scip".to_owned(),
                version: "1.0".to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        documents: vec![pb::Document {
            relative_path: SOURCE_PATH.to_owned(),
            text: source.to_owned(),
            language: "rust".to_owned(),
            position_encoding: EnumOrUnknown::new(
                pb::PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            occurrences: vec![
                occurrence(0, "target", TARGET, true, false),
                occurrence(1, "caller", CALLER, true, false),
                occurrence(1, "target", TARGET, false, true),
            ],
            symbols: [(TARGET, "target"), (CALLER, "caller")]
                .into_iter()
                .map(|(symbol, name)| pb::SymbolInformation {
                    symbol: symbol.to_owned(),
                    display_name: name.to_owned(),
                    kind: EnumOrUnknown::new(pb::symbol_information::Kind::Function),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let bytes = message.write_to_bytes().unwrap();
    let hash = ContentHash::of(source.as_bytes());
    let manifest = ScipImportManifest {
        source_revision: revision,
        artifact_hash: ContentHash::of(&bytes),
        tool_name: "synthetic-scip".to_owned(),
        tool_version: "1.0".to_owned(),
        compiler_identity: ContentHash::of(b"KNOWELL_CANARY_SYNTHETIC_COMPILER_INPUTS"),
        build_inputs: BTreeMap::from([(path(SOURCE_PATH), hash)]),
        documents: BTreeMap::from([(path(SOURCE_PATH), hash)]),
    };
    PreparedScipImport::from_bytes(&bytes, manifest, &ScipImportLimits::default()).unwrap()
}

async fn precise_edges(db: &TestDb, pin: knowell_store::GenerationPin) -> Vec<graph::Edge> {
    let mut conn = db.conn().await;
    graph::edges_with_origins(&mut conn, pin, &[format!("scip:{SOURCE_PATH}")])
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_import_survives_restart_and_only_its_generation_has_precise_relations() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let ws = fixture_workspace(Some(&[PROJECT]));
    ws.write(PROJECT, SOURCE_PATH, SOURCE);
    let revision = ws.commit_all(PROJECT, "synthetic precise source");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let settings = config(data.path());
    let indexer = indexer_with(&db, settings.clone(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let initial = active_pin(&db, view).await;
    let prepared = artifact(SOURCE, revision);
    indexer
        .rebuild_view_with_scip(view, &prepared, Priority::Interactive)
        .await
        .unwrap();
    drop(indexer);

    let restarted = indexer_with(&db, settings, &embedder);
    restarted.register(&ws.resolved).await.unwrap();
    let summary = restarted.run_until_idle_scoped_with(1).await.unwrap();
    assert_eq!(summary.failed, 0);
    let imported = active_pin(&db, view).await;
    assert!(imported.generation > initial.generation);
    let edges = precise_edges(&db, imported).await;
    assert!(edges.iter().any(|edge| edge.edge.kind == "calls"
        && edge.edge.evidence_type == EvidenceType::SemanticResolved));
    assert!(edges.iter().any(|edge| edge.edge.kind == "references"));
    assert!(
        edges
            .iter()
            .all(|edge| edge.edge.evidence["generation"] == imported.generation)
    );
    let mut conn = db.conn().await;
    let coverage = knowell_store::analysis::coverage_at(
        &mut conn,
        registration.organization,
        imported,
        &path(SOURCE_PATH),
        &ContentHash::of(SOURCE.as_bytes()),
    )
    .await
    .unwrap();
    assert!(
        coverage
            .iter()
            .any(|item| item.provider == "scip" && item.details["calls_written"] == 1)
    );
    let occurrences: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM occurrence WHERE view_id = $1 AND origin LIKE 'scip:%'
         AND valid_from <= $2 AND (valid_to IS NULL OR valid_to > $2)",
    )
    .bind(view)
    .bind(imported.generation)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(occurrences, 1);
    drop(conn);

    // The defining source stays identical: dependency/context changes still
    // invalidate the old compiler observation, independently of text hashes.
    ws.write(
        PROJECT,
        "docs/precise-input.md",
        "synthetic build input changed\n",
    );
    ws.commit_all(PROJECT, "advance without a new compiler artifact");
    restarted
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    assert_eq!(
        restarted
            .run_until_idle_scoped_with(1)
            .await
            .unwrap()
            .failed,
        0
    );
    let next = active_pin(&db, view).await;
    assert!(next.generation > imported.generation);
    assert!(precise_edges(&db, next).await.is_empty());
    assert!(!precise_edges(&db, imported).await.is_empty());
    let mut conn = db.conn().await;
    let current = knowell_store::analysis::coverage_at(
        &mut conn,
        registration.organization,
        next,
        &path(SOURCE_PATH),
        &ContentHash::of(SOURCE.as_bytes()),
    )
    .await
    .unwrap();
    assert!(current.iter().all(|item| item.provider != "scip"));
    let occurrences: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM occurrence WHERE view_id = $1 AND origin LIKE 'scip:%'
         AND valid_from <= $2 AND (valid_to IS NULL OR valid_to > $2)",
    )
    .bind(view)
    .bind(next.generation)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(occurrences, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_revision_is_rejected_without_queueing_or_activation() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let ws = fixture_workspace(Some(&[PROJECT]));
    ws.write(PROJECT, SOURCE_PATH, SOURCE);
    ws.commit_all(PROJECT, "synthetic precise source");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer_with(&db, config(data.path()), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let initial = active_pin(&db, view).await;
    let prepared = artifact(SOURCE, "b".repeat(40));
    assert!(
        indexer
            .rebuild_view_with_scip(view, &prepared, Priority::Interactive)
            .await
            .is_err()
    );
    assert_eq!(indexer.run_until_idle_scoped_with(1).await.unwrap().jobs, 0);
    assert_eq!(active_pin(&db, view).await, initial);
    assert!(precise_edges(&db, initial).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_document_hash_fails_the_build_and_keeps_the_previous_generation() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let ws = fixture_workspace(Some(&[PROJECT]));
    ws.write(PROJECT, SOURCE_PATH, SOURCE);
    let revision = ws.commit_all(PROJECT, "synthetic precise source");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let mut settings = config(data.path());
    settings.jobs.max_attempts = 1;
    let indexer = indexer_with(&db, settings, &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let initial = active_pin(&db, view).await;
    let stale_source = "fn target() { let old = 1; }\nfn caller() { target(); }\n";
    let prepared = artifact(stale_source, revision);
    indexer
        .rebuild_view_with_scip(view, &prepared, Priority::Interactive)
        .await
        .unwrap();
    let summary = indexer.run_until_idle_scoped_with(1).await.unwrap();
    assert!(summary.failed > 0);
    assert_eq!(active_pin(&db, view).await, initial);
    let mut conn = db.conn().await;
    let generations = knowell_store::views::list_generations(&mut conn, view)
        .await
        .unwrap();
    assert!(
        generations
            .iter()
            .any(|generation| generation.generation > initial.generation
                && generation.state == GenerationState::Failed)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coverage_failure_rolls_back_compiler_replacement_before_dead_build_cleanup() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let ws = fixture_workspace(Some(&[PROJECT]));
    ws.write(PROJECT, SOURCE_PATH, SOURCE);
    let revision = ws.commit_all(PROJECT, "synthetic precise source");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let mut settings = config(data.path());
    // Inspect the first failed attempt while its generation is still building;
    // dead-build cleanup must not conceal a non-atomic compiler replacement.
    settings.jobs.max_attempts = 2;
    let indexer = indexer_with(&db, settings, &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let prepared = artifact(SOURCE, revision);
    indexer
        .rebuild_view_with_scip(view, &prepared, Priority::Interactive)
        .await
        .unwrap();
    assert_eq!(
        indexer.run_until_idle_scoped_with(1).await.unwrap().failed,
        0
    );
    let imported = active_pin(&db, view).await;
    let old_edges = precise_edges(&db, imported).await;
    assert!(!old_edges.is_empty());
    assert!(old_edges.iter().all(|edge| edge.valid_to.is_none()));

    let mut conn = db.conn().await;
    let old_coverage = knowell_store::analysis::coverage_at(
        &mut conn,
        registration.organization,
        imported,
        &path(SOURCE_PATH),
        &ContentHash::of(SOURCE.as_bytes()),
    )
    .await
    .unwrap();
    assert!(
        old_coverage
            .iter()
            .any(|coverage| coverage.provider == "scip")
    );
    let old_occurrences: Vec<(uuid::Uuid, i64, Option<i64>)> = sqlx::query_as(
        "SELECT id, valid_from, valid_to FROM occurrence
         WHERE view_id = $1 AND origin LIKE 'scip:%' ORDER BY id",
    )
    .bind(view)
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert_eq!(old_occurrences.len(), 1);
    assert!(old_occurrences.iter().all(|(_, _, end)| end.is_none()));

    // Sequences do not roll back: this canary proves the injected failure was
    // reached after both replacement writes and historical interval closures.
    // All objects live solely in this test's disposable database.
    sqlx::query("CREATE SEQUENCE knowell_canary_scip_coverage_attempts")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "CREATE FUNCTION knowell_canary_reject_scip_coverage() RETURNS trigger
         LANGUAGE plpgsql AS $$
         BEGIN
           IF NEW.provider = 'scip' THEN
             IF NOT EXISTS (SELECT 1 FROM edge WHERE view_id = NEW.view_id
                 AND valid_from = NEW.generation AND origin LIKE 'scip:%')
                OR NOT EXISTS (SELECT 1 FROM occurrence WHERE view_id = NEW.view_id
                 AND valid_from = NEW.generation AND origin LIKE 'scip:%')
                OR NOT EXISTS (SELECT 1 FROM edge WHERE view_id = NEW.view_id
                 AND valid_from < NEW.generation AND valid_to = NEW.generation
                 AND origin LIKE 'scip:%')
                OR NOT EXISTS (SELECT 1 FROM occurrence WHERE view_id = NEW.view_id
                 AND valid_from < NEW.generation AND valid_to = NEW.generation
                 AND origin LIKE 'scip:%') THEN
               RAISE EXCEPTION 'KNOWELL_CANARY_MISSING_COMPILER_REPLACEMENT';
             END IF;
             PERFORM nextval('knowell_canary_scip_coverage_attempts');
             RAISE EXCEPTION 'KNOWELL_CANARY_REJECT_AFTER_COMPILER_REPLACEMENT';
           END IF;
           RETURN NEW;
         END;
         $$",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER knowell_canary_reject_scip_coverage
         BEFORE INSERT OR UPDATE ON analysis_coverage
         FOR EACH ROW EXECUTE FUNCTION knowell_canary_reject_scip_coverage()",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);

    indexer
        .rebuild_view_with_scip(view, &prepared, Priority::Interactive)
        .await
        .unwrap();
    assert!(indexer.run_next_job().await.unwrap()); // T0 prepares the generation.
    assert!(indexer.run_next_job().await.unwrap()); // T1 fails after compiler writes.
    assert_eq!(active_pin(&db, view).await, imported);
    let mut conn = db.conn().await;
    let building = knowell_store::views::building_generation(&mut conn, view)
        .await
        .unwrap()
        .unwrap();
    assert!(building > imported.generation);
    let (attempts, reached): (i64, bool) =
        sqlx::query_as("SELECT last_value, is_called FROM knowell_canary_scip_coverage_attempts")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert!(reached);
    assert_eq!(attempts, 1);
    let replacement_edges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM edge WHERE view_id = $1 AND valid_from = $2 AND origin LIKE 'scip:%'",
    )
    .bind(view)
    .bind(building)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(replacement_edges, 0);
    let occurrences: Vec<(uuid::Uuid, i64, Option<i64>)> = sqlx::query_as(
        "SELECT id, valid_from, valid_to FROM occurrence
         WHERE view_id = $1 AND origin LIKE 'scip:%' ORDER BY id",
    )
    .bind(view)
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert_eq!(occurrences, old_occurrences);
    let replacement_coverage: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM analysis_coverage WHERE view_id = $1 AND generation = $2 AND provider = 'scip'",
    ).bind(view).bind(building).fetch_one(&mut *conn).await.unwrap();
    assert_eq!(replacement_coverage, 0);
    let coverage = knowell_store::analysis::coverage_at(
        &mut conn,
        registration.organization,
        imported,
        &path(SOURCE_PATH),
        &ContentHash::of(SOURCE.as_bytes()),
    )
    .await
    .unwrap();
    assert_eq!(coverage, old_coverage);
    drop(conn);
    assert_eq!(precise_edges(&db, imported).await, old_edges);

    // Make the synthetic retry runnable without a timing-dependent sleep.
    let mut conn = db.conn().await;
    let due =
        sqlx::query("UPDATE job SET run_after = now() WHERE view_id = $1 AND state = 'failed'")
            .bind(view)
            .execute(&mut *conn)
            .await
            .unwrap()
            .rows_affected();
    assert_eq!(due, 1);
    drop(conn);
    let summary = indexer.run_until_idle_scoped_with(1).await.unwrap();
    assert_eq!(summary.failed, 1);
    assert_eq!(active_pin(&db, view).await, imported);
    assert_eq!(precise_edges(&db, imported).await, old_edges);
    let mut conn = db.conn().await;
    let generation = knowell_store::views::get_generation(&mut conn, view, building)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(generation.state, GenerationState::Failed);
    let coverage = knowell_store::analysis::coverage_at(
        &mut conn,
        registration.organization,
        imported,
        &path(SOURCE_PATH),
        &ContentHash::of(SOURCE.as_bytes()),
    )
    .await
    .unwrap();
    assert_eq!(coverage, old_coverage);
}
