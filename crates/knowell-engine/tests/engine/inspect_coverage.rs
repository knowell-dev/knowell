//! Inspection consumes bounded source observations without implying an exhaustive inventory.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use knowell_core::{ContentHash, LineRange, RepoPath};
use knowell_engine::Engine;
use knowell_index::Priority;
use knowell_mcp::tools::{
    AnalyzeImpactInput, ChangeSubject, FetchInput, FlowDirection, InspectSymbolInput,
    InspectSymbolOutput, OpenWorkspaceInput, SymbolFacet, TraceFlowInput,
};
use knowell_mcp::{
    AnalysisLevel, EvidenceType, FileLocator, GapReason, KnowellTools, RelationKind, SymbolRef,
    Target,
};
use knowell_store::graph::{NewEdge, NodeRef};
use knowell_store::symbols::NewOccurrence;
use knowell_store::views::GenerationPin;
use knowell_store::{OccurrenceRole, OrganizationId, ProjectId, ViewId};
use knowell_store::{analysis, content, graph, symbols, views};

use crate::common::{
    TestDb, Workspace, access, alice_caller, fixture_workspace, git_available, indexer_config,
    name, require_db,
};

const PROJECT: &str = "billing-api";
const DEFINITION: &str = "src/inspect-probe.ts";
const CALLER: &str = "src/inspect-caller.ts";
const IMPLEMENTATION: &str = "src/inspect-implementation.ts";
const TEST: &str = "tests/inspect-probe.test.ts";
const IMPORT_ONLY: &str = "tests/inspect-import-only.test.ts";
const MANY: &str = "src/inspect-many.ts";

fn path(value: &str) -> RepoPath {
    RepoPath::new(value).unwrap()
}

fn write(ws: &Workspace, relative: &str, text: &str) {
    let file = ws.project_dir(PROJECT).join(relative);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

fn workspace(include_many: bool) -> Workspace {
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == PROJECT);
    let project = &mut ws.resolved.projects[0];
    project.embedding.provider = None;
    project.embedding.model = None;
    write(
        &ws,
        DEFINITION,
        "export class InspectProbe {\n  run(value: number): number { return value; }\n}\n",
    );
    write(
        &ws,
        CALLER,
        "import { InspectProbe } from './inspect-probe';\nexport function inspectCaller() { return new InspectProbe().run(2); }\n",
    );
    write(
        &ws,
        IMPLEMENTATION,
        "import { InspectProbe } from './inspect-probe';\nexport class InspectChild implements InspectProbe { run(value: number) { return value; } }\n",
    );
    write(
        &ws,
        TEST,
        "import { InspectProbe } from '../src/inspect-probe';\nexport function inspectTest() { return new InspectProbe().run(3) === 3; }\n",
    );
    write(
        &ws,
        IMPORT_ONLY,
        "import '../src/inspect-probe';\nexport const untouched = 1;\n",
    );
    if include_many {
        let many = (1..=300)
            .map(|number| format!("export const inspectMany{number} = InspectProbe;\n"))
            .collect::<String>();
        write(&ws, MANY, &many);
    }
    write(
        &ws,
        "package.json",
        "{\"name\":\"synthetic-inspect\",\"version\":\"1.0.0\"}\n",
    );
    ws.commit_all(PROJECT, "add synthetic source inspection fixtures");
    ws
}

async fn indexed_engine(db: &TestDb, ws: &Workspace, data: &Path) -> Engine {
    let engine = Engine::builder(db.store.clone(), indexer_config(data))
        .workspace(ws.resolved.clone())
        .access(Arc::new(access()))
        .build()
        .await
        .unwrap();
    let (_, outcomes) = engine
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    assert!(
        outcomes
            .iter()
            .all(|outcome| !matches!(outcome, knowell_index::SyncOutcome::Failed { .. })),
        "{outcomes:?}"
    );
    engine
}

struct Imported {
    pin: GenerationPin,
    commit: String,
}

/// Synthetic compiler observations exercise the imported proof contract; no compiler/provider runs.
async fn import(db: &TestDb, ws: &Workspace, include_many: bool) -> Imported {
    let mut conn = db.store.acquire().await.unwrap();
    let (organization, project, view, generation): (OrganizationId, ProjectId, ViewId, i64) =
        sqlx::query_as("SELECT p.organization_id, p.id, v.id, v.active_generation FROM project p JOIN view v ON v.project_id = p.id WHERE p.name = 'billing-api' AND v.track_target = 'branch:main'")
            .fetch_one(&mut *conn).await.unwrap();
    let old = GenerationPin { view, generation };
    let definitions = symbols::definitions_in_paths(&mut conn, old, &[path(DEFINITION)])
        .await
        .unwrap();
    let symbol = definitions
        .iter()
        .find(|definition| definition.symbol.qualified_name.ends_with("#InspectProbe"))
        .unwrap()
        .symbol
        .id;
    let commit = ws.git(PROJECT, &["rev-parse", "HEAD"]);
    let generation = views::begin_generation(&mut conn, view, Some(&commit))
        .await
        .unwrap();
    let pin = GenerationPin { view, generation };
    let files = content::files_at(&mut conn, pin).await.unwrap();
    let digest = ContentHash::of(b"synthetic compiler and artifact identity");
    for relative in [DEFINITION, CALLER, IMPLEMENTATION, TEST, MANY] {
        if relative == MANY && !include_many {
            continue;
        }
        let file = files
            .iter()
            .find(|file| file.path == path(relative))
            .unwrap();
        let hash = file.content_hash;
        let identity = serde_json::json!({
            "analysis_kind":"scip", "analysis_format":1, "source_revision":commit,
            "view":view, "generation":generation, "path":relative, "content_hash":hash,
            "artifact_hash":digest, "analysis_input_hash":digest, "compiler_identity":digest,
            "encoding_unknown":false, "source_unavailable":false,
            "syntax_truncated":false, "syntax_unavailable":false,
            "references_complete":false, "calls_complete":false,
        });
        analysis::upsert_coverage(
            &mut conn,
            organization,
            pin,
            &path(relative),
            &hash,
            "scip",
            &identity,
        )
        .await
        .unwrap();
        if relative == DEFINITION {
            continue;
        }
        let origin = format!("scip:{relative}");
        let line_numbers: Vec<u32> = if relative == MANY {
            (1..=300).collect()
        } else {
            vec![2]
        };
        let occurrences = line_numbers
            .iter()
            .map(|line| NewOccurrence {
                symbol,
                path: path(relative),
                content_hash: hash,
                lines: LineRange::new(*line, *line).unwrap(),
                role: OccurrenceRole::Reference,
            })
            .collect::<Vec<_>>();
        analysis::replace_scip_occurrences(&mut conn, organization, pin, &origin, &occurrences)
            .await
            .unwrap();
        let relations: &[&str] = match relative {
            TEST if include_many => &[],
            CALLER | TEST => &["references", "calls"],
            IMPLEMENTATION => &["implements"],
            _ => &[],
        };
        let edges = relations
            .iter()
            .map(|kind| {
                let mut evidence = identity.clone();
                evidence["lines"] = serde_json::json!([2, 2]);
                NewEdge {
                    from: NodeRef::File {
                        project,
                        path: path(relative),
                    },
                    to: NodeRef::Symbol(symbol),
                    kind: (*kind).to_owned(),
                    evidence_type: knowell_store::EvidenceType::SemanticResolved,
                    resolution: knowell_store::Resolution::Resolved,
                    evidence,
                    origin: origin.clone(),
                }
            })
            .collect::<Vec<_>>();
        graph::replace_edges(&mut conn, view, generation, &[origin], &edges)
            .await
            .unwrap();
    }
    // This navigational lead points at a module, and does not prove a test of its class.
    let import = NewEdge {
        from: NodeRef::File {
            project,
            path: path(IMPORT_ONLY),
        },
        to: NodeRef::File {
            project,
            path: path(DEFINITION),
        },
        kind: "imports".to_owned(),
        evidence_type: knowell_store::EvidenceType::Syntactic,
        resolution: knowell_store::Resolution::Resolved,
        evidence: serde_json::json!({"lines":[1,1]}),
        origin: IMPORT_ONLY.to_owned(),
    };
    graph::replace_edges(
        &mut conn,
        view,
        generation,
        &[IMPORT_ONLY.to_owned()],
        &[import],
    )
    .await
    .unwrap();
    views::activate_generation(&mut conn, view, generation)
        .await
        .unwrap();
    Imported { pin, commit }
}

async fn inspect(
    engine: &Engine,
    target: Target,
    limit: u32,
    include: Vec<SymbolFacet>,
) -> InspectSymbolOutput {
    engine
        .inspect_symbol(
            &alice_caller(),
            InspectSymbolInput {
                target,
                symbol: SymbolRef {
                    symbol: Some("InspectProbe".to_owned()),
                    project: Some(name(PROJECT)),
                    ..SymbolRef::default()
                },
                include,
                limit: Some(limit),
            },
        )
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exact_compiler_observations_are_deduplicated_and_never_claim_complete_coverage() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace(false);
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let imported = import(&db, &ws, false).await;
    let opened = engine
        .open_workspace(&alice_caller(), OpenWorkspaceInput::default())
        .await
        .unwrap();
    let old_target = Target::context(opened.context_id);
    let result = inspect(&engine, old_target.clone(), 20, Vec::new()).await;
    let info = result
        .symbols
        .iter()
        .find(|symbol| symbol.name == "InspectProbe")
        .unwrap();
    assert_eq!(info.analysis, AnalysisLevel::Semantic);
    assert!(!info.references_complete);
    let calls = info
        .references
        .iter()
        .filter(|link| {
            link.evidence.path == path(CALLER)
                && link.evidence.lines == LineRange::new(2, 2).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].relation, RelationKind::Calls);
    assert_eq!(calls[0].evidence_type, EvidenceType::SemanticallyResolved);
    assert_eq!(calls[0].evidence.lines, LineRange::new(2, 2).unwrap());
    assert_eq!(calls[0].evidence.commit.as_str(), imported.commit);
    assert!(info.references.iter().any(|link| {
        link.evidence.path == path(CALLER)
            && link.relation == RelationKind::Imports
            && link.evidence.lines == LineRange::new(1, 1).unwrap()
    }));
    assert!(
        info.implementations
            .iter()
            .any(|link| link.relation == RelationKind::Implements
                && link.evidence.path == path(IMPLEMENTATION))
    );
    assert!(
        info.tests
            .iter()
            .any(|link| link.relation == RelationKind::Calls && link.evidence.path == path(TEST))
    );
    assert!(
        info.tests.iter().all(|link| {
            link.evidence.path != path(TEST) || link.evidence.lines != LineRange::new(1, 1).unwrap()
        }),
        "an import binding alone must not become a test association"
    );
    assert!(
        info.tests
            .iter()
            .all(|link| link.evidence.path != path(IMPORT_ONLY))
    );
    assert!(
        info.references
            .iter()
            .any(|link| link.relation == RelationKind::Imports
                && link.evidence.path == path(IMPORT_ONLY))
    );
    assert!(
        result
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::RelationsNotReady
                && gap.message.contains("inventory is partial"))
    );

    // Unchanged source bytes cannot revalidate inherited compiler inputs in a newer generation.
    let mut conn = db.store.acquire().await.unwrap();
    let generation = views::begin_generation(&mut conn, imported.pin.view, Some(&imported.commit))
        .await
        .unwrap();
    views::activate_generation(&mut conn, imported.pin.view, generation)
        .await
        .unwrap();
    drop(conn);
    let current = engine
        .open_workspace(&alice_caller(), OpenWorkspaceInput::default())
        .await
        .unwrap();
    let current_result =
        inspect(&engine, Target::context(current.context_id), 20, Vec::new()).await;
    let current_info = current_result
        .symbols
        .iter()
        .find(|symbol| symbol.name == "InspectProbe")
        .unwrap();
    assert_ne!(current_info.analysis, AnalysisLevel::Semantic);
    assert!(
        current_info
            .references
            .iter()
            .chain(&current_info.implementations)
            .chain(&current_info.tests)
            .all(|link| link.evidence_type != EvidenceType::SemanticallyResolved)
    );
    let retained = inspect(&engine, old_target, 20, Vec::new()).await;
    assert!(
        retained
            .symbols
            .iter()
            .flat_map(|symbol| &symbol.references)
            .any(
                |link| link.evidence_type == EvidenceType::SemanticallyResolved
                    && link.relation == RelationKind::Calls
            )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dependency_only_personal_changes_exclude_base_compiler_links_in_unchanged_files() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace(false);
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    import(&db, &ws, false).await;
    let shared = engine
        .open_workspace(&alice_caller(), OpenWorkspaceInput::default())
        .await
        .unwrap();
    let worktree = ws.dir.path().join("inspect-personal");
    ws.git(
        PROJECT,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/synthetic-inspect-personal",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::write(
        worktree.join("package.json"),
        "{\"name\":\"synthetic-inspect\",\"version\":\"2.0.0\"}\n",
    )
    .unwrap();
    let personal = engine
        .open_workspace(
            &alice_caller(),
            OpenWorkspaceInput {
                working_directory: Some(worktree.to_string_lossy().into_owned()),
                ..OpenWorkspaceInput::default()
            },
        )
        .await
        .unwrap();
    let personal_manifest = personal.manifest.clone();
    assert!(
        personal_manifest.iter().any(|project| {
            project.project == name(PROJECT)
                && project.layer == knowell_mcp::ViewLayer::Personal
                && project.local_generation > 0
        }),
        "{personal_manifest:?}"
    );
    let result = inspect(
        &engine,
        Target::context(personal.context_id),
        20,
        Vec::new(),
    )
    .await;
    let info = result
        .symbols
        .iter()
        .find(|symbol| symbol.name == "InspectProbe")
        .unwrap();
    assert_ne!(info.analysis, AnalysisLevel::Semantic);
    assert!(!info.references_complete);
    assert!(
        info.references
            .iter()
            .chain(&info.implementations)
            .chain(&info.tests)
            .all(|link| link.evidence_type != EvidenceType::SemanticallyResolved)
    );
    assert!(
        info.references
            .iter()
            .any(|link| link.relation == RelationKind::Imports),
        "ordinary import navigation must survive; info={info:?}; gaps={:?}; manifest={personal_manifest:?}",
        result.gaps
    );
    assert!(result.gaps.iter().any(|gap| {
        gap.message
            .contains("unchanged file text does not validate changed dependency inputs")
    }));
    let shared_result = inspect(&engine, Target::context(shared.context_id), 20, Vec::new()).await;
    assert!(
        shared_result
            .symbols
            .iter()
            .flat_map(|symbol| &symbol.references)
            .any(|link| link.evidence_type == EvidenceType::SemanticallyResolved)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspection_pages_occurrences_and_caps_facets_after_deduplicating_call_spans() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace(true);
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    import(&db, &ws, true).await;
    let opened = engine
        .open_workspace(&alice_caller(), OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = Target::context(opened.context_id);
    let result = inspect(&engine, target.clone(), 200, vec![SymbolFacet::References]).await;
    let info = result
        .symbols
        .iter()
        .find(|symbol| symbol.name == "InspectProbe")
        .unwrap();
    assert_eq!(info.references.len(), 200);
    assert!(info.implementations.is_empty() && info.tests.is_empty() && info.signature.is_none());
    assert_eq!(
        info.references
            .iter()
            .filter(|link| link.evidence.path == path(CALLER))
            .count(),
        1
    );
    assert!(
        info.references
            .iter()
            .filter(|link| link.evidence.path == path(CALLER))
            .all(|link| link.relation == RelationKind::Calls)
    );
    let unique = info
        .references
        .iter()
        .map(|link| (&link.evidence.path, link.evidence.lines))
        .collect::<BTreeSet<_>>();
    assert_eq!(unique.len(), info.references.len());
    assert!(
        result
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::LimitReached
                && gap.message.contains("reference links omitted"))
    );
    assert!(!info.references_complete);
    let repeated = inspect(&engine, target.clone(), 200, vec![SymbolFacet::References]).await;
    assert_eq!(result, repeated);
    // This occurrence sorts after the 300 earlier source references and has no graph edge.
    // It can only appear when acquisition follows the occurrence cursor beyond its first page.
    let tests = inspect(&engine, target, 20, vec![SymbolFacet::Tests]).await;
    assert!(
        tests
            .symbols
            .iter()
            .flat_map(|symbol| &symbol.tests)
            .any(|link| link.evidence.path == path(TEST)
                && link.relation == RelationKind::References
                && link.evidence_type == EvidenceType::SemanticallyResolved)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graph_source_ids_do_not_substitute_a_new_commit_with_identical_definition_bytes() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace(false);
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    import(&db, &ws, false).await;
    let opened = engine
        .open_workspace(&alice_caller(), OpenWorkspaceInput::default())
        .await
        .unwrap();
    let old_target = Target::context(opened.context_id);
    let original = inspect(&engine, old_target.clone(), 20, Vec::new()).await;
    let old_info = original
        .symbols
        .iter()
        .find(|symbol| symbol.name == "InspectProbe")
        .unwrap();
    let old_id = old_info.id.clone();
    let old_hash = old_info.definition.content_hash;
    write(
        &ws,
        "package.json",
        "{\"name\":\"synthetic-inspect\",\"version\":\"3.0.0\"}\n",
    );
    let current_commit = ws.commit_all(PROJECT, "change only synthetic dependency inputs");
    let (_, outcomes) = engine
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    assert!(
        outcomes
            .iter()
            .all(|outcome| !matches!(outcome, knowell_index::SyncOutcome::Failed { .. })),
        "{outcomes:?}"
    );
    let current = engine
        .open_workspace(&alice_caller(), OpenWorkspaceInput::default())
        .await
        .unwrap();
    let current_target = Target::context(current.context_id);
    let inspected = inspect(&engine, current_target.clone(), 20, Vec::new()).await;
    let current_info = inspected
        .symbols
        .iter()
        .find(|symbol| symbol.name == "InspectProbe")
        .unwrap();
    assert_eq!(current_info.definition.content_hash, old_hash);
    assert_ne!(current_info.id, old_id);
    assert_eq!(current_info.definition.commit.as_str(), current_commit);

    let stale = engine
        .trace_flow(
            &alice_caller(),
            TraceFlowInput {
                target: current_target.clone(),
                id: Some(old_id.clone()),
                direction: Some(FlowDirection::Upstream),
                ..TraceFlowInput::default()
            },
        )
        .await
        .unwrap();
    assert!(stale.nodes.is_empty() && stale.edges.is_empty());
    assert!(
        stale
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::NotFound)
    );
    let impact = engine
        .analyze_impact(
            &alice_caller(),
            AnalyzeImpactInput {
                target: current_target.clone(),
                change: Some(ChangeSubject::Symbol {
                    symbol: SymbolRef {
                        id: Some(old_id.clone()),
                        ..SymbolRef::default()
                    },
                }),
                ..AnalyzeImpactInput::default()
            },
        )
        .await
        .unwrap();
    assert!(impact.changed.is_empty() && impact.impacted.is_empty());
    let fresh = engine
        .trace_flow(
            &alice_caller(),
            TraceFlowInput {
                target: current_target,
                id: Some(current_info.id.clone()),
                direction: Some(FlowDirection::Upstream),
                ..TraceFlowInput::default()
            },
        )
        .await
        .unwrap();
    assert!(fresh.nodes.iter().any(|node| {
        node.evidence.as_ref().is_some_and(|evidence| {
            evidence.path == path(DEFINITION) && evidence.commit.as_str() == current_commit
        })
    }));
    let retained = engine
        .trace_flow(
            &alice_caller(),
            TraceFlowInput {
                target: old_target,
                id: Some(old_id),
                direction: Some(FlowDirection::Upstream),
                ..TraceFlowInput::default()
            },
        )
        .await
        .unwrap();
    assert!(retained.nodes.iter().any(|node| {
        node.evidence.as_ref().is_some_and(|evidence| {
            evidence.path == path(DEFINITION) && evidence.commit == old_info.definition.commit
        })
    }));
    assert!(
        retained
            .edges
            .iter()
            .any(|edge| edge.relation == RelationKind::Calls)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proven_calls_and_syntactic_import_navigation_survive_heuristic_reference_crowding() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace(true);
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    // MANY has only normal heuristic observations, rather than precise imported references.
    import(&db, &ws, false).await;
    let opened = engine
        .open_workspace(&alice_caller(), OpenWorkspaceInput::default())
        .await
        .unwrap();
    let result = inspect(&engine, Target::context(opened.context_id), 20, Vec::new()).await;
    let info = result
        .symbols
        .iter()
        .find(|symbol| symbol.name == "InspectProbe")
        .unwrap();
    assert_eq!(info.references.len(), 20);
    assert!(
        info.references
            .iter()
            .any(|link| link.evidence.path == path(CALLER)
                && link.evidence.lines == LineRange::new(2, 2).unwrap()
                && link.relation == RelationKind::Calls
                && link.evidence_type == EvidenceType::SemanticallyResolved)
    );
    assert!(
        info.references
            .iter()
            .any(|link| link.evidence.path == path(IMPORT_ONLY)
                && link.evidence.lines == LineRange::new(1, 1).unwrap()
                && link.relation == RelationKind::Imports
                && link.evidence_type == EvidenceType::SyntacticObservation)
    );
    assert!(
        info.references
            .iter()
            .any(|link| link.evidence.path == path(MANY)
                && link.evidence_type == EvidenceType::HeuristicMatch)
    );
    assert!(
        result
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::LimitReached
                && gap.message.contains("reference links omitted"))
    );
    assert!(
        info.tests
            .iter()
            .all(|link| link.evidence.path != path(IMPORT_ONLY))
    );
    assert!(!info.references_complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_line_source_ids_preserve_ambiguity_and_navigation_requires_a_qualified_symbol() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace(false);
    let same_line = "src/inspect-same-line.ts";
    write(
        &ws,
        same_line,
        "export class InlineScope {\n  inlineAlpha() { return 1; } inlineBeta() { return 2; }\n}\n",
    );
    let overlapping_line = "src/inspect-overlapping-line.ts";
    write(
        &ws,
        overlapping_line,
        "export function overlapAlpha() {\n  return 1;\n} export function overlapBeta() {\n  return 2;\n}\n",
    );
    ws.commit_all(PROJECT, "add synthetic methods sharing one source line");
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let opened = engine
        .open_workspace(&alice_caller(), OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = Target::context(opened.context_id);
    let mut definitions = Vec::new();
    for qualified in ["InlineScope.inlineAlpha", "InlineScope.inlineBeta"] {
        let result = engine
            .inspect_symbol(
                &alice_caller(),
                InspectSymbolInput {
                    target: target.clone(),
                    symbol: SymbolRef {
                        symbol: Some(qualified.to_owned()),
                        project: Some(name(PROJECT)),
                        ..SymbolRef::default()
                    },
                    include: vec![SymbolFacet::Signature],
                    ..InspectSymbolInput::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result.symbols.len(), 1, "{result:?}");
        let definition = result.symbols.into_iter().next().unwrap();
        assert_eq!(definition.qualified_name, qualified);
        assert_eq!(definition.definition.path, path(same_line));
        assert_eq!(definition.definition.lines, LineRange::new(2, 2).unwrap());
        definitions.push(definition);
    }
    assert_eq!(definitions[0].id, definitions[1].id);
    let source_id = definitions[0].id.clone();
    let ambiguous = engine
        .inspect_symbol(
            &alice_caller(),
            InspectSymbolInput {
                target: target.clone(),
                symbol: SymbolRef {
                    id: Some(source_id.clone()),
                    ..SymbolRef::default()
                },
                include: vec![SymbolFacet::Signature],
                ..InspectSymbolInput::default()
            },
        )
        .await
        .unwrap();
    let names = ambiguous
        .symbols
        .iter()
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names,
        BTreeSet::from(["InlineScope.inlineAlpha", "InlineScope.inlineBeta"])
    );
    assert!(
        ambiguous
            .symbols
            .iter()
            .all(|symbol| symbol.id == source_id)
    );
    assert!(ambiguous.gaps.iter().any(|gap| {
        gap.reason == GapReason::LimitReached
            && gap
                .message
                .contains("choose a qualified symbol and project")
    }));
    let navigation = engine
        .trace_flow(
            &alice_caller(),
            TraceFlowInput {
                target: target.clone(),
                id: Some(source_id),
                navigation: Some(true),
                ..TraceFlowInput::default()
            },
        )
        .await
        .unwrap();
    assert!(navigation.edges.is_empty());
    let candidates = navigation
        .nodes
        .iter()
        .filter_map(|node| {
            node.evidence
                .as_ref()
                .and_then(|evidence| evidence.symbol.as_deref())
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        candidates,
        BTreeSet::from(["InlineScope.inlineAlpha", "InlineScope.inlineBeta"])
    );
    assert!(
        navigation
            .gaps
            .iter()
            .any(|gap| gap.message.contains("did not choose one or expand"))
    );
    let chosen = engine
        .trace_flow(
            &alice_caller(),
            TraceFlowInput {
                target: target.clone(),
                symbol: Some("InlineScope.inlineBeta".to_owned()),
                project: Some(name(PROJECT)),
                navigation: Some(true),
                ..TraceFlowInput::default()
            },
        )
        .await
        .unwrap();
    assert!(chosen.nodes.first().is_some_and(|node| {
        node.evidence
            .as_ref()
            .and_then(|evidence| evidence.symbol.as_deref())
            == Some("InlineScope.inlineBeta")
    }));
    assert!(
        chosen
            .gaps
            .iter()
            .all(|gap| !gap.message.contains("equally ranked start definitions"))
    );
    // A source fragment at the shared closing/opening line has no exact definition
    // span. Both equally small enclosing declarations must remain candidates.
    let fragment = engine
        .fetch(
            &alice_caller(),
            FetchInput {
                target: target.clone(),
                paths: vec![FileLocator {
                    project: name(PROJECT),
                    path: path(overlapping_line),
                    lines: Some(LineRange::new(3, 3).unwrap()),
                }],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(fragment.items.len(), 1);
    let enclosing = engine
        .inspect_symbol(
            &alice_caller(),
            InspectSymbolInput {
                target,
                symbol: SymbolRef {
                    id: Some(fragment.items[0].id.clone()),
                    ..SymbolRef::default()
                },
                include: vec![SymbolFacet::Signature],
                ..InspectSymbolInput::default()
            },
        )
        .await
        .unwrap();
    let enclosing_names = enclosing
        .symbols
        .iter()
        .map(|symbol| symbol.name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        enclosing_names,
        BTreeSet::from(["overlapAlpha", "overlapBeta"])
    );
}
