//! Admission of compiler-produced edges at an immutable source generation.

use knowell_core::{ContentHash, RepoPath};
use knowell_store::EvidenceType;
use knowell_store::analysis::AnalysisCoverage;
use knowell_store::graph::Edge;
use knowell_store::views::GenerationPin;

/// Source path for a SCIP edge whose import identity is well formed and whose
/// producing generation equals this snapshot. Historical imports remain useful
/// only in their own generation; inherited edges are never compiler evidence for
/// a later dependency/configuration state.
pub(crate) fn scip_source_path(edge: &Edge, pin: GenerationPin) -> Option<RepoPath> {
    let evidence = &edge.edge.evidence;
    if edge.view != pin.view
        || edge.edge.evidence_type != EvidenceType::SemanticResolved
        || evidence.get("analysis_kind")?.as_str()? != "scip"
        || evidence.get("analysis_format")?.as_u64()? != 1
        || evidence.get("generation")?.as_i64()? != pin.generation
        || evidence.get("view")?.as_str()? != pin.view.to_string()
    {
        return None;
    }
    let path = RepoPath::new(evidence.get("path")?.as_str()?).ok()?;
    if edge.edge.origin != format!("scip:{path}") {
        return None;
    }
    for key in [
        "content_hash",
        "artifact_hash",
        "analysis_input_hash",
        "compiler_identity",
    ] {
        evidence.get(key)?.as_str()?.parse::<ContentHash>().ok()?;
    }
    let revision = evidence.get("source_revision")?.as_str()?;
    if !matches!(revision.len(), 40 | 64)
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some(path)
}

/// Keeps normal edges under their existing admission rules, and SCIP edges only
/// when their compiler observation describes the exact pinned source hash.
pub(crate) fn admits_scip_edge(edge: &Edge, pin: GenerationPin, source_hash: ContentHash) -> bool {
    let marked = edge.edge.origin.starts_with("scip:")
        || edge
            .edge
            .evidence
            .get("analysis_kind")
            .and_then(serde_json::Value::as_str)
            == Some("scip");
    if !marked {
        return true;
    }
    scip_source_path(edge, pin).is_some()
        && edge
            .edge
            .evidence
            .get("content_hash")
            .and_then(serde_json::Value::as_str)
            .and_then(|hash| hash.parse::<ContentHash>().ok())
            == Some(source_hash)
}

/// Availability of a precise import for this exact source. This validates the
/// imported input identity and position/syntax usability; it never establishes
/// exhaustive references, implementations or calls.
pub(crate) fn admits_scip_coverage(
    record: &AnalysisCoverage,
    pin: GenerationPin,
    commit: &str,
    source_hash: ContentHash,
) -> bool {
    if record.provider != "scip" {
        return false;
    }
    let details = &record.details;
    if details
        .get("source_revision")
        .and_then(serde_json::Value::as_str)
        != Some(commit)
        || details.get("view").and_then(serde_json::Value::as_str)
            != Some(pin.view.to_string().as_str())
        || details
            .get("generation")
            .and_then(serde_json::Value::as_i64)
            != Some(pin.generation)
        || details
            .get("content_hash")
            .and_then(serde_json::Value::as_str)
            .and_then(|hash| hash.parse::<ContentHash>().ok())
            != Some(source_hash)
    {
        return false;
    }
    for key in ["artifact_hash", "analysis_input_hash", "compiler_identity"] {
        if details
            .get(key)
            .and_then(serde_json::Value::as_str)
            .and_then(|hash| hash.parse::<ContentHash>().ok())
            .is_none()
        {
            return false;
        }
    }
    for key in [
        "encoding_unknown",
        "source_unavailable",
        "syntax_truncated",
        "syntax_unavailable",
    ] {
        if details.get(key).and_then(serde_json::Value::as_bool) != Some(false) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use knowell_store::graph::{NewEdge, NodeRef};
    use knowell_store::{EdgeId, ProjectId, Resolution, ViewId};

    use super::*;

    fn fixture() -> (Edge, GenerationPin, ContentHash) {
        let pin = GenerationPin {
            view: ViewId(uuid::Uuid::from_u128(1)),
            generation: 4,
        };
        let hash = ContentHash::of(b"synthetic source");
        let edge = Edge {
            id: EdgeId(uuid::Uuid::from_u128(2)),
            view: pin.view,
            edge: NewEdge {
                from: NodeRef::Project(ProjectId(uuid::Uuid::from_u128(3))),
                to: NodeRef::Project(ProjectId(uuid::Uuid::from_u128(3))),
                kind: "references".to_owned(),
                evidence_type: EvidenceType::SemanticResolved,
                resolution: Resolution::Resolved,
                evidence: serde_json::json!({
                    "analysis_kind": "scip", "analysis_format": 1,
                    "view": pin.view, "generation": pin.generation,
                    "path": "src/lib.rs", "content_hash": hash,
                    "artifact_hash": hash, "analysis_input_hash": hash,
                    "compiler_identity": hash, "source_revision": "a".repeat(40),
                }),
                origin: "scip:src/lib.rs".to_owned(),
            },
            valid_from: 4,
            valid_to: None,
        };
        (edge, pin, hash)
    }

    #[test]
    fn rejects_carried_or_wrong_source_compiler_edges() {
        let (edge, pin, hash) = fixture();
        assert!(admits_scip_edge(&edge, pin, hash));
        assert_eq!(
            scip_source_path(&edge, pin),
            Some(RepoPath::new("src/lib.rs").unwrap())
        );
        assert!(!admits_scip_edge(
            &edge,
            GenerationPin {
                generation: 5,
                ..pin
            },
            hash
        ));
        assert!(!admits_scip_edge(
            &edge,
            pin,
            ContentHash::of(b"changed source")
        ));
        let mut forged = edge.clone();
        forged.edge.evidence["path"] = serde_json::json!("../secret.env");
        assert!(!admits_scip_edge(&forged, pin, hash));
        forged = edge.clone();
        forged.edge.evidence["compiler_identity"] = serde_json::json!("unknown");
        assert!(!admits_scip_edge(&forged, pin, hash));
        forged = edge;
        forged.edge.origin = "src/lib.rs".to_owned();
        assert!(!admits_scip_edge(&forged, pin, hash));
    }

    #[test]
    fn ordinary_edges_keep_existing_admission() {
        let (mut edge, pin, hash) = fixture();
        edge.edge.origin = "src/lib.rs".to_owned();
        edge.edge.evidence_type = EvidenceType::Syntactic;
        edge.edge.evidence = serde_json::json!({"path": "src/lib.rs"});
        assert!(admits_scip_edge(&edge, pin, hash));
    }

    #[test]
    fn compiler_coverage_requires_current_identity_and_usable_positions() {
        let (_, pin, hash) = fixture();
        let commit = "a".repeat(40);
        let mut record = AnalysisCoverage {
            provider: "scip".to_owned(),
            details: serde_json::json!({
                "view": pin.view, "generation": pin.generation, "content_hash": hash,
                "source_revision": commit, "artifact_hash": hash,
                "analysis_input_hash": hash, "compiler_identity": hash,
                "encoding_unknown": false, "source_unavailable": false,
                "syntax_truncated": false, "syntax_unavailable": false,
                "references_complete": false,
            }),
        };
        assert!(admits_scip_coverage(&record, pin, &commit, hash));
        assert!(!admits_scip_coverage(
            &record,
            GenerationPin {
                generation: 5,
                ..pin
            },
            &commit,
            hash
        ));
        assert!(!admits_scip_coverage(&record, pin, &"b".repeat(40), hash));
        record.details["encoding_unknown"] = serde_json::json!(true);
        assert!(!admits_scip_coverage(&record, pin, &commit, hash));
        record.details["encoding_unknown"] = serde_json::json!(false);
        record.details["compiler_identity"] = serde_json::json!("missing");
        assert!(!admits_scip_coverage(&record, pin, &commit, hash));
    }
}
