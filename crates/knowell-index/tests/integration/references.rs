//! Reference occurrences and edges, and the re-resolution of unchanged
//! files when the files they import appear or disappear.

use std::sync::Arc;

use knowell_index::Priority;
use knowell_store::graph::{self, Edge, NodeRef};
use knowell_store::symbols;
use knowell_store::views::GenerationPin;
use knowell_store::{EvidenceType, OccurrenceRole, ProjectId, Resolution, SymbolId, ViewId};

use crate::common::{
    CountingEmbedder, TestDb, active_pin, fixture_workspace, git_available, indexer, path,
    require_db, view_of,
};

const PROJECT: &str = "billing-api";

async fn symbol(db: &TestDb, project: ProjectId, key: &str) -> SymbolId {
    let mut conn = db.conn().await;
    let found = symbols::find_symbols(&mut conn, project, key)
        .await
        .unwrap();
    assert_eq!(found.len(), 1, "{key}: {found:?}");
    found[0].id
}

async fn edges(db: &TestDb, node: &NodeRef, pin: GenerationPin, kind: &str) -> Vec<Edge> {
    let mut conn = db.conn().await;
    graph::edges_at(&mut conn, node, &[pin])
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.edge.kind == kind)
        .collect()
}

async fn refresh(indexer: &knowell_index::Indexer<CountingEmbedder>, view: ViewId) {
    indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    indexer.run_until_idle().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn references_are_recorded_with_their_evidence_and_imports_follow_the_files() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let ws = fixture_workspace(Some(&[PROJECT]));
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let project = registration.views[0].project_id;

    ws.write(
        PROJECT,
        "src/refs/util.ts",
        "export function knowellRefHelper(x: number): number {\n  return x + 1;\n}\n\nexport function knowellRefFormat(v: string): string {\n  return v;\n}\n",
    );
    ws.write(
        PROJECT,
        "src/refs/other.ts",
        "export function knowellRefFormat(v: string): string {\n  return v.trim();\n}\n",
    );
    ws.write(
        PROJECT,
        "src/refs/sibling.ts",
        "export function knowellRefSibling(): number {\n  return 7;\n}\n",
    );
    ws.write(
        PROJECT,
        "src/refs/main.ts",
        "import { knowellRefHelper, knowellRefFormat } from './util';\nimport { knowellRefFormat as otherFormat } from './other';\n\nexport function knowellRefMain(): number {\n  knowellRefFormat('a');\n  otherFormat('b');\n  knowellRefSibling();\n  return knowellRefHelper(1);\n}\n",
    );
    ws.write(
        PROJECT,
        "src/refs/waiting.ts",
        "import { knowellRefLate } from './later';\n\nexport function knowellRefWaiting(): number {\n  return knowellRefLate();\n}\n",
    );
    ws.commit_all(PROJECT, "reference probes");
    refresh(&indexer, view).await;
    let pin = active_pin(&db, view).await;

    let helper = symbol(&db, project, "src/refs/util.ts#knowellRefHelper").await;
    let main = symbol(&db, project, "src/refs/main.ts#knowellRefMain").await;
    let sibling = symbol(&db, project, "src/refs/sibling.ts#knowellRefSibling").await;
    let util_format = symbol(&db, project, "src/refs/util.ts#knowellRefFormat").await;
    let other_format = symbol(&db, project, "src/refs/other.ts#knowellRefFormat").await;

    // An imported, unique name: a syntactic, resolved edge and an occurrence.
    let into_helper = edges(&db, &NodeRef::Symbol(helper), pin, "references").await;
    let from_main: Vec<&Edge> = into_helper
        .iter()
        .filter(|e| e.edge.from == NodeRef::Symbol(main))
        .collect();
    assert_eq!(from_main.len(), 1, "{into_helper:?}");
    assert_eq!(from_main[0].edge.evidence_type, EvidenceType::Syntactic);
    assert_eq!(from_main[0].edge.resolution, Resolution::Resolved);
    assert_eq!(from_main[0].edge.origin, "src/refs/main.ts");
    let mut conn = db.conn().await;
    let occurrences = symbols::occurrences_of(&mut conn, helper, &[pin])
        .await
        .unwrap();
    assert!(
        occurrences
            .iter()
            .any(|o| o.occurrence.role == OccurrenceRole::Reference
                && o.occurrence.path == path("src/refs/main.ts")
                && o.occurrence.lines.start() == 8),
        "{occurrences:?}"
    );
    drop(conn);

    // The same name defined by two imported files: ambiguous edges to both,
    // and no reference occurrence presenting a guess as a reference.
    for target in [util_format, other_format] {
        let into = edges(&db, &NodeRef::Symbol(target), pin, "references").await;
        let from_main: Vec<&Edge> = into
            .iter()
            .filter(|e| e.edge.from == NodeRef::Symbol(main))
            .collect();
        assert_eq!(from_main.len(), 1, "{into:?}");
        assert_eq!(from_main[0].edge.resolution, Resolution::Ambiguous);
        let mut conn = db.conn().await;
        assert!(
            symbols::occurrences_of(&mut conn, target, &[pin])
                .await
                .unwrap()
                .iter()
                .all(|o| o.occurrence.role == OccurrenceRole::Definition)
        );
    }
    // A name only a file of the same directory defines: heuristic.
    let into_sibling = edges(&db, &NodeRef::Symbol(sibling), pin, "references").await;
    assert!(
        into_sibling
            .iter()
            .any(|e| e.edge.from == NodeRef::Symbol(main)
                && e.edge.evidence_type == EvidenceType::Heuristic),
        "{into_sibling:?}"
    );

    // `./later` does not exist yet: the import is unresolved.
    let waiting = NodeRef::File {
        project,
        path: path("src/refs/waiting.ts"),
    };
    let imports = edges(&db, &waiting, pin, "imports").await;
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].edge.resolution, Resolution::Unresolved);

    // Adding the imported file re-resolves the unchanged importer.
    let stats = indexer.stats();
    ws.write(
        PROJECT,
        "src/refs/later.ts",
        "export function knowellRefLate(): number {\n  return 9;\n}\n",
    );
    ws.commit_all(PROJECT, "add the late file");
    refresh(&indexer, view).await;
    let after = indexer.stats();
    assert!(after.dependents_reresolved > stats.dependents_reresolved);
    let pin = active_pin(&db, view).await;
    let imports = edges(&db, &waiting, pin, "imports").await;
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].edge.resolution, Resolution::Resolved);
    assert_eq!(
        imports[0].edge.to,
        NodeRef::File {
            project,
            path: path("src/refs/later.ts")
        }
    );
    let late = symbol(&db, project, "src/refs/later.ts#knowellRefLate").await;
    let waiting_fn = symbol(&db, project, "src/refs/waiting.ts#knowellRefWaiting").await;
    assert!(
        edges(&db, &NodeRef::Symbol(late), pin, "references")
            .await
            .iter()
            .any(|e| e.edge.from == NodeRef::Symbol(waiting_fn)
                && e.edge.evidence_type == EvidenceType::Syntactic)
    );

    // Deleting an imported file leaves no dangling edges in its importer.
    ws.git(PROJECT, &["rm", "-q", "src/refs/util.ts"]);
    ws.commit_all(PROJECT, "remove util");
    refresh(&indexer, view).await;
    let pin = active_pin(&db, view).await;
    let main_file = NodeRef::File {
        project,
        path: path("src/refs/main.ts"),
    };
    let imports = edges(&db, &main_file, pin, "imports").await;
    assert!(
        imports.iter().any(|e| e.edge.to
            == NodeRef::Name {
                project,
                name: "./util".to_owned()
            }
            && e.edge.resolution == Resolution::Unresolved),
        "{imports:?}"
    );
    assert!(
        edges(&db, &NodeRef::Symbol(helper), pin, "references")
            .await
            .is_empty(),
        "no reference into a removed definition"
    );
    drop(indexer);
}
