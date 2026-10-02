//! Conflict detection between accepted records.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{RecordId, Subject};
use crate::model::{KnowledgeRecord, RecordState, Scope};

/// Two accepted records that disagree. Reported, never merged.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct Conflict {
    /// The shared subject.
    pub subject: Subject,
    /// The record with the smaller id.
    pub first: RecordId,
    /// Scope of `first`.
    pub first_scope: Scope,
    /// The record with the larger id.
    pub second: RecordId,
    /// Scope of `second`.
    pub second_scope: Scope,
}

/// Collapses whitespace so formatting-only differences are not conflicts.
fn canonical_body(body: &str) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Finds every pair of **accepted** records that share a subject, have
/// overlapping scopes (see [`Scope::overlaps`]) and different bodies
/// (compared ignoring whitespace differences).
///
/// Records in other states are ignored: only accepted records are current,
/// and only current records can contradict each other. The result is sorted
/// by subject and record ids, so it is deterministic.
pub fn detect_conflicts(records: &[KnowledgeRecord]) -> Vec<Conflict> {
    let mut by_subject: BTreeMap<&Subject, Vec<&KnowledgeRecord>> = BTreeMap::new();
    for record in records.iter().filter(|r| r.state == RecordState::Accepted) {
        by_subject.entry(&record.subject).or_default().push(record);
    }
    let mut out = Vec::new();
    for (subject, mut group) in by_subject {
        group.sort_by_key(|r| r.id);
        for (i, a) in group.iter().enumerate() {
            for b in group.iter().skip(i + 1) {
                if a.id != b.id
                    && a.scope.overlaps(&b.scope)
                    && canonical_body(&a.body) != canonical_body(&b.body)
                {
                    out.push(Conflict {
                        subject: subject.clone(),
                        first: a.id,
                        first_scope: a.scope.clone(),
                        second: b.id,
                        second_scope: b.scope.clone(),
                    });
                }
            }
        }
    }
    out
}

/// Conflicts that accepting `candidate` would create with `existing` records.
///
/// Use before accepting a proposal so the reviewer sees the contradiction.
/// `candidate` itself is not required to be accepted yet.
pub fn conflicts_for_candidate(
    existing: &[KnowledgeRecord],
    candidate: &KnowledgeRecord,
) -> Vec<Conflict> {
    let mut out = Vec::new();
    for other in existing {
        if other.state != RecordState::Accepted
            || other.id == candidate.id
            || other.subject != candidate.subject
            || !other.scope.overlaps(&candidate.scope)
            || canonical_body(&other.body) == canonical_body(&candidate.body)
        {
            continue;
        }
        let (a, b) = if other.id < candidate.id {
            (other, candidate)
        } else {
            (candidate, other)
        };
        out.push(Conflict {
            subject: a.subject.clone(),
            first: a.id,
            first_scope: a.scope.clone(),
            second: b.id,
            second_scope: b.scope.clone(),
        });
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, RecordKind};
    use crate::policy::{AcceptancePolicy, Rights};
    use crate::testutil::*;

    fn accepted(n: u128, scope: Scope, subject: &str, body: &str) -> KnowledgeRecord {
        let mut new = new_record(RecordKind::Human, n);
        new.scope = scope;
        new.subject = Subject::new(subject).unwrap();
        new.body = body.to_string();
        KnowledgeRecord::write(
            new,
            human("a"),
            Rights::REVIEWER,
            &AcceptancePolicy::default(),
            ts(1),
        )
        .unwrap()
        .record
    }

    #[test]
    fn same_subject_overlapping_scope_different_body_conflicts() {
        let a = accepted(1, project_scope("api"), "pay.retry", "retry three times");
        let b = accepted(2, project_scope("api"), "pay.retry", "never retry");
        let conflicts = detect_conflicts(&[b.clone(), a.clone()]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!((conflicts[0].first, conflicts[0].second), (rid(1), rid(2)));
        assert_eq!(conflicts[0].subject.as_str(), "pay.retry");
    }

    #[test]
    fn broader_scope_overlaps_narrower() {
        let org = accepted(1, Scope::Organization, "pay.retry", "a");
        let ws = accepted(2, Scope::Workspace(name("shop")), "pay.retry", "b");
        let proj = accepted(3, project_scope("api"), "pay.retry", "c");
        let other_ws_proj = accepted(
            4,
            Scope::Project {
                workspace: name("other"),
                project: name("x"),
            },
            "pay.retry",
            "d",
        );
        let conflicts = detect_conflicts(&[org, ws, proj, other_ws_proj]);
        let pairs: Vec<_> = conflicts.iter().map(|c| (c.first, c.second)).collect();
        // org-ws, org-proj, org-other, ws-proj. ws-other and proj-other do not overlap.
        assert_eq!(
            pairs,
            vec![
                (rid(1), rid(2)),
                (rid(1), rid(3)),
                (rid(1), rid(4)),
                (rid(2), rid(3))
            ]
        );
    }

    #[test]
    fn no_conflict_when_equivalent_disjoint_or_not_accepted() {
        let a = accepted(1, project_scope("api"), "pay.retry", "retry  three\ntimes");
        let same = accepted(2, project_scope("api"), "pay.retry", "retry three times");
        assert!(
            detect_conflicts(&[a.clone(), same]).is_empty(),
            "whitespace-only differences are not conflicts"
        );

        let sibling = accepted(3, project_scope("web"), "pay.retry", "different");
        assert!(
            detect_conflicts(&[a.clone(), sibling]).is_empty(),
            "sibling projects do not overlap"
        );

        let other_subject = accepted(4, project_scope("api"), "pay.timeout", "different");
        assert!(detect_conflicts(&[a.clone(), other_subject]).is_empty());

        let mut stale = accepted(5, project_scope("api"), "pay.retry", "different");
        stale.mark_stale(&Actor::System, "x", ts(2)).unwrap();
        assert!(detect_conflicts(&[a.clone(), stale]).is_empty());

        let proposed = KnowledgeRecord::propose(
            {
                let mut n = new_record(RecordKind::Human, 6);
                n.subject = Subject::new("pay.retry").unwrap();
                n.body = "different".into();
                n
            },
            human("a"),
            ts(1),
        )
        .unwrap();
        assert!(detect_conflicts(&[a, proposed]).is_empty());
    }

    #[test]
    fn private_scopes_never_conflict_with_others() {
        let user = accepted(
            1,
            Scope::User(crate::ids::UserId::new("u").unwrap()),
            "pay.retry",
            "mine",
        );
        let org = accepted(2, Scope::Organization, "pay.retry", "theirs");
        assert!(detect_conflicts(&[user, org]).is_empty());
    }

    #[test]
    fn candidate_check_names_existing_conflicts() {
        let existing = vec![accepted(
            10,
            project_scope("api"),
            "pay.retry",
            "retry three times",
        )];
        let candidate = KnowledgeRecord::propose(
            {
                let mut n = new_record(RecordKind::Human, 5);
                n.subject = Subject::new("pay.retry").unwrap();
                n.body = "never retry".into();
                n
            },
            human("a"),
            ts(1),
        )
        .unwrap();
        let found = conflicts_for_candidate(&existing, &candidate);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].first, found[0].second), (rid(5), rid(10)));
        assert!(conflicts_for_candidate(&existing, &existing[0]).is_empty());
    }
}
