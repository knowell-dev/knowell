//! Staleness: which accepted records stop being true when code changes.
//!
//! Stale records are kept, flagged and queued for revalidation. They are
//! never deleted and never presented as current.

use std::collections::BTreeMap;

use knowell_core::{ContentHash, Name, RepoPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::KnowledgeError;
use crate::ids::{RecordId, SymbolId, Timestamp};
use crate::model::{Actor, KnowledgeRecord, RecordState};

/// What happened to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    /// The file now has this content hash.
    Modified {
        /// Hash of the new content.
        new_hash: ContentHash,
    },
    /// The file no longer exists (also used for renames: the old path is gone).
    Deleted,
}

/// A changed or deleted file, as reported by the indexer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileChange {
    /// The project the file belongs to.
    pub project: Name,
    /// File path relative to the project root.
    pub path: RepoPath,
    /// The hash the indexer saw before the change. A modification whose new
    /// hash equals the old one is a no-op and is ignored.
    pub old_hash: ContentHash,
    /// What happened.
    pub kind: FileChangeKind,
}

/// What happened to a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolChangeKind {
    /// The symbol's definition or signature changed.
    Changed,
    /// The symbol no longer exists (also used for renames).
    Removed,
}

/// A changed or removed symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SymbolChange {
    /// The symbol.
    pub symbol: SymbolId,
    /// What happened.
    pub kind: SymbolChangeKind,
}

/// The set of changes between two index states.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangeSet {
    /// File changes.
    pub files: Vec<FileChange>,
    /// Symbol changes.
    pub symbols: Vec<SymbolChange>,
}

/// Why a record is considered stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    /// An evidence file changed and no longer matches the recorded hash.
    FileChanged {
        /// Project of the file.
        project: Name,
        /// Path of the file.
        path: RepoPath,
    },
    /// An evidence file was deleted.
    FileDeleted {
        /// Project of the file.
        project: Name,
        /// Path of the file.
        path: RepoPath,
    },
    /// A related symbol changed.
    SymbolChanged(SymbolId),
    /// A related symbol was removed.
    SymbolRemoved(SymbolId),
}

impl StaleReason {
    fn describe(&self) -> String {
        match self {
            StaleReason::FileChanged { project, path } => {
                format!("evidence file {project}:{path} changed")
            }
            StaleReason::FileDeleted { project, path } => {
                format!("evidence file {project}:{path} was deleted")
            }
            StaleReason::SymbolChanged(s) => format!("related symbol {s} changed"),
            StaleReason::SymbolRemoved(s) => format!("related symbol {s} was removed"),
        }
    }
}

/// One affected record and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StaleFinding {
    /// The affected record.
    pub record: RecordId,
    /// Why, in evidence order then symbol order. Never empty.
    pub reasons: Vec<StaleReason>,
}

/// Result of [`compute_staleness`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StalenessReport {
    /// Accepted records that become stale, sorted by record id.
    pub stale: Vec<StaleFinding>,
    /// Proposed records whose evidence also changed. They are not flagged
    /// (they were never current) but the reviewer should know. Sorted.
    pub proposed_affected: Vec<StaleFinding>,
}

/// Computes which records are affected by `changes`.
///
/// * An **accepted** record is stale if any of its evidence files was deleted
///   or now has a different hash than the evidence recorded, or if any of its
///   related symbols changed or was removed.
/// * A **proposed** record with such evidence goes into
///   [`StalenessReport::proposed_affected`].
/// * Records in other states are ignored: stale ones are already flagged,
///   rejected and superseded ones are history.
///
/// Pure and deterministic; the output is sorted by record id.
pub fn compute_staleness(records: &[KnowledgeRecord], changes: &ChangeSet) -> StalenessReport {
    let mut files: BTreeMap<(&Name, &RepoPath), &FileChange> = BTreeMap::new();
    for change in &changes.files {
        if let FileChangeKind::Modified { new_hash } = change.kind
            && new_hash == change.old_hash
        {
            continue;
        }
        files.insert((&change.project, &change.path), change);
    }
    let symbols: BTreeMap<&SymbolId, SymbolChangeKind> = changes
        .symbols
        .iter()
        .map(|c| (&c.symbol, c.kind))
        .collect();

    let mut report = StalenessReport::default();
    for record in records {
        if !matches!(record.state, RecordState::Accepted | RecordState::Proposed) {
            continue;
        }
        let mut reasons = Vec::new();
        for ev in &record.evidence {
            let Some(change) = files.get(&(&ev.project, &ev.path)) else {
                continue;
            };
            match change.kind {
                FileChangeKind::Deleted => reasons.push(StaleReason::FileDeleted {
                    project: ev.project.clone(),
                    path: ev.path.clone(),
                }),
                FileChangeKind::Modified { new_hash } if new_hash != ev.content_hash => {
                    reasons.push(StaleReason::FileChanged {
                        project: ev.project.clone(),
                        path: ev.path.clone(),
                    });
                }
                FileChangeKind::Modified { .. } => {}
            }
        }
        for sym in &record.related_symbols {
            match symbols.get(sym) {
                Some(SymbolChangeKind::Changed) => {
                    reasons.push(StaleReason::SymbolChanged(sym.clone()))
                }
                Some(SymbolChangeKind::Removed) => {
                    reasons.push(StaleReason::SymbolRemoved(sym.clone()))
                }
                None => {}
            }
        }
        if reasons.is_empty() {
            continue;
        }
        let finding = StaleFinding {
            record: record.id,
            reasons,
        };
        if record.state == RecordState::Accepted {
            report.stale.push(finding);
        } else {
            report.proposed_affected.push(finding);
        }
    }
    report.stale.sort_by_key(|f| f.record);
    report.proposed_affected.sort_by_key(|f| f.record);
    report
}

/// Result of [`apply_staleness`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// Records that were flagged stale.
    pub marked: Vec<RecordId>,
    /// Records that could not be flagged (not found, or changed state since
    /// the report was computed), with the reason.
    pub skipped: Vec<(RecordId, Option<KnowledgeError>)>,
}

/// Flags every record named in `report.stale` as stale (actor: the engine).
///
/// A record that is not found, or that is no longer accepted, is reported in
/// [`ApplyOutcome::skipped`] rather than silently ignored.
pub fn apply_staleness(
    records: &mut [KnowledgeRecord],
    report: &StalenessReport,
    at: Timestamp,
) -> ApplyOutcome {
    let mut outcome = ApplyOutcome::default();
    for finding in &report.stale {
        let Some(record) = records.iter_mut().find(|r| r.id == finding.record) else {
            outcome.skipped.push((finding.record, None));
            continue;
        };
        let reason = finding
            .reasons
            .iter()
            .map(StaleReason::describe)
            .collect::<Vec<_>>()
            .join("; ");
        match record.mark_stale(&Actor::System, &reason, at) {
            Ok(()) => outcome.marked.push(finding.record),
            Err(err) => outcome.skipped.push((finding.record, Some(err))),
        }
    }
    outcome
}

/// A stale record waiting to be revalidated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RevalidationItem {
    /// The stale record.
    pub record: RecordId,
    /// Why it is stale (accumulated if queued more than once).
    pub reasons: Vec<StaleReason>,
    /// When it was first queued.
    pub queued_at: Timestamp,
}

/// The queue of records awaiting revalidation. Pure data; storage persists it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RevalidationQueue {
    items: Vec<RevalidationItem>,
}

impl RevalidationQueue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues every record of `report.stale`. A record already queued keeps
    /// its original `queued_at` and gains any new reasons.
    pub fn enqueue(&mut self, report: &StalenessReport, at: Timestamp) {
        for finding in &report.stale {
            match self.items.iter_mut().find(|i| i.record == finding.record) {
                Some(item) => {
                    for reason in &finding.reasons {
                        if !item.reasons.contains(reason) {
                            item.reasons.push(reason.clone());
                        }
                    }
                }
                None => self.items.push(RevalidationItem {
                    record: finding.record,
                    reasons: finding.reasons.clone(),
                    queued_at: at,
                }),
            }
        }
        self.items.sort_by_key(|i| (i.queued_at, i.record));
    }

    /// Pending items, oldest first, ties broken by record id.
    pub fn pending(&self) -> &[RevalidationItem] {
        &self.items
    }

    /// Removes a record from the queue (after revalidation, supersession or
    /// rejection). Returns whether it was queued.
    pub fn resolve(&mut self, record: RecordId) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.record != record);
        self.items.len() != before
    }

    /// Number of pending items.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is pending.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RecordKind;
    use crate::policy::{AcceptancePolicy, Rights};
    use crate::testutil::*;

    fn rec(n: u128, ev: Vec<crate::model::Evidence>, symbols: Vec<SymbolId>) -> KnowledgeRecord {
        let mut new = new_record(RecordKind::Human, n);
        new.evidence = ev;
        new.related_symbols = symbols;
        let out = KnowledgeRecord::write(
            new,
            human("a"),
            Rights::REVIEWER,
            &AcceptancePolicy::default(),
            ts(1),
        )
        .unwrap();
        out.record
    }

    fn modified(path: &str, old: &str, new: &str) -> FileChange {
        FileChange {
            project: name("api"),
            path: RepoPath::new(path).unwrap(),
            old_hash: hash(old),
            kind: FileChangeKind::Modified {
                new_hash: hash(new),
            },
        }
    }

    fn deleted(path: &str, old: &str) -> FileChange {
        FileChange {
            project: name("api"),
            path: RepoPath::new(path).unwrap(),
            old_hash: hash(old),
            kind: FileChangeKind::Deleted,
        }
    }

    use knowell_core::RepoPath;

    #[test]
    fn modified_file_makes_matching_record_stale() {
        let records = vec![
            rec(2, vec![evidence("src/pay.rs", "h1")], vec![]),
            rec(1, vec![evidence("src/other.rs", "h1")], vec![]),
        ];
        let changes = ChangeSet {
            files: vec![modified("src/pay.rs", "h1", "h2")],
            symbols: vec![],
        };
        let report = compute_staleness(&records, &changes);
        assert_eq!(report.stale.len(), 1);
        assert_eq!(report.stale[0].record, rid(2));
        assert!(matches!(
            report.stale[0].reasons[0],
            StaleReason::FileChanged { .. }
        ));
    }

    #[test]
    fn deleted_file_makes_record_stale() {
        let records = vec![rec(1, vec![evidence("src/pay.rs", "h1")], vec![])];
        let report = compute_staleness(
            &records,
            &ChangeSet {
                files: vec![deleted("src/pay.rs", "h1")],
                symbols: vec![],
            },
        );
        assert!(matches!(
            report.stale[0].reasons[0],
            StaleReason::FileDeleted { .. }
        ));
    }

    #[test]
    fn unchanged_or_noop_changes_do_not_flag() {
        let records = vec![rec(1, vec![evidence("src/pay.rs", "h1")], vec![])];
        // Hash unchanged (no-op).
        let noop = ChangeSet {
            files: vec![modified("src/pay.rs", "h1", "h1")],
            symbols: vec![],
        };
        assert!(compute_staleness(&records, &noop).stale.is_empty());
        // File already at the hash the evidence recorded (evidence re-pointed to the new version).
        let same = ChangeSet {
            files: vec![modified("src/pay.rs", "h0", "h1")],
            symbols: vec![],
        };
        assert!(compute_staleness(&records, &same).stale.is_empty());
        // Different project with the same path.
        let mut other = modified("src/pay.rs", "h1", "h2");
        other.project = name("web");
        assert!(
            compute_staleness(
                &records,
                &ChangeSet {
                    files: vec![other],
                    symbols: vec![]
                }
            )
            .stale
            .is_empty()
        );
    }

    #[test]
    fn symbol_changes_flag_records() {
        let records = vec![
            rec(1, vec![evidence("a.rs", "h1")], vec![sym("pay::charge")]),
            rec(2, vec![evidence("b.rs", "h1")], vec![sym("pay::refund")]),
            rec(3, vec![evidence("c.rs", "h1")], vec![sym("pay::other")]),
        ];
        let changes = ChangeSet {
            files: vec![],
            symbols: vec![
                SymbolChange {
                    symbol: sym("pay::charge"),
                    kind: SymbolChangeKind::Changed,
                },
                SymbolChange {
                    symbol: sym("pay::refund"),
                    kind: SymbolChangeKind::Removed,
                },
            ],
        };
        let report = compute_staleness(&records, &changes);
        assert_eq!(report.stale.len(), 2);
        assert_eq!(
            report.stale[0].reasons,
            vec![StaleReason::SymbolChanged(sym("pay::charge"))]
        );
        assert_eq!(
            report.stale[1].reasons,
            vec![StaleReason::SymbolRemoved(sym("pay::refund"))]
        );
    }

    #[test]
    fn only_accepted_and_proposed_records_are_considered() {
        let accepted = rec(1, vec![evidence("src/pay.rs", "h1")], vec![]);
        let mut stale_already = rec(2, vec![evidence("src/pay.rs", "h1")], vec![]);
        stale_already
            .mark_stale(&Actor::System, "x", ts(2))
            .unwrap();
        let proposed = KnowledgeRecord::propose(
            {
                let mut n = new_record(RecordKind::Human, 3);
                n.evidence = vec![evidence("src/pay.rs", "h1")];
                n
            },
            human("a"),
            ts(1),
        )
        .unwrap();
        let mut rejected = rec(4, vec![evidence("src/pay.rs", "h1")], vec![]);
        rejected
            .reject(&human("r"), Rights::REVIEWER, "no", ts(2))
            .unwrap();
        let records = vec![accepted, stale_already, proposed, rejected];
        let report = compute_staleness(
            &records,
            &ChangeSet {
                files: vec![deleted("src/pay.rs", "h1")],
                symbols: vec![],
            },
        );
        assert_eq!(
            report.stale.iter().map(|f| f.record).collect::<Vec<_>>(),
            vec![rid(1)]
        );
        assert_eq!(
            report
                .proposed_affected
                .iter()
                .map(|f| f.record)
                .collect::<Vec<_>>(),
            vec![rid(3)]
        );
    }

    #[test]
    fn report_is_sorted_and_deterministic() {
        let records: Vec<_> = [5u128, 3, 9, 1]
            .iter()
            .map(|n| rec(*n, vec![evidence("src/pay.rs", "h1")], vec![]))
            .collect();
        let changes = ChangeSet {
            files: vec![modified("src/pay.rs", "h1", "h2")],
            symbols: vec![],
        };
        let a = compute_staleness(&records, &changes);
        let mut reversed = records.clone();
        reversed.reverse();
        let b = compute_staleness(&reversed, &changes);
        assert_eq!(a, b);
        let ids: Vec<_> = a.stale.iter().map(|f| f.record).collect();
        assert_eq!(ids, vec![rid(1), rid(3), rid(5), rid(9)]);
    }

    #[test]
    fn apply_marks_stale_keeps_records_and_logs_reason() {
        let mut records = vec![rec(1, vec![evidence("src/pay.rs", "h1")], vec![])];
        let report = compute_staleness(
            &records,
            &ChangeSet {
                files: vec![modified("src/pay.rs", "h1", "h2")],
                symbols: vec![],
            },
        );
        let outcome = apply_staleness(&mut records, &report, ts(50));
        assert_eq!(outcome.marked, vec![rid(1)]);
        assert!(outcome.skipped.is_empty());
        assert_eq!(records.len(), 1, "never deleted");
        assert_eq!(records[0].state, RecordState::Stale);
        assert!(!records[0].is_current());
        let last = records[0].history.last().unwrap();
        assert_eq!(last.actor, Actor::System);
        assert!(last.reason.contains("src/pay.rs"));
    }

    #[test]
    fn apply_reports_skipped_records() {
        let mut records = vec![rec(1, vec![evidence("src/pay.rs", "h1")], vec![])];
        let report = compute_staleness(
            &records,
            &ChangeSet {
                files: vec![deleted("src/pay.rs", "h1")],
                symbols: vec![],
            },
        );
        // The record moves on before the report is applied.
        records[0]
            .reject(&human("r"), Rights::REVIEWER, "no", ts(2))
            .unwrap();
        let outcome = apply_staleness(&mut records, &report, ts(50));
        assert!(outcome.marked.is_empty());
        assert!(matches!(
            outcome.skipped[0].1,
            Some(KnowledgeError::IllegalTransition { .. })
        ));
        let outcome = apply_staleness(&mut [], &report, ts(50));
        assert_eq!(outcome.skipped, vec![(rid(1), None)]);
    }

    #[test]
    fn queue_enqueue_merge_resolve() {
        let mut records = vec![rec(1, vec![evidence("src/pay.rs", "h1")], vec![sym("s")])];
        let r1 = compute_staleness(
            &records,
            &ChangeSet {
                files: vec![modified("src/pay.rs", "h1", "h2")],
                symbols: vec![],
            },
        );
        let mut q = RevalidationQueue::new();
        assert!(q.is_empty());
        q.enqueue(&r1, ts(10));
        let r2 = compute_staleness(
            &records,
            &ChangeSet {
                files: vec![],
                symbols: vec![SymbolChange {
                    symbol: sym("s"),
                    kind: SymbolChangeKind::Removed,
                }],
            },
        );
        q.enqueue(&r2, ts(20));
        assert_eq!(q.len(), 1);
        assert_eq!(q.pending()[0].queued_at, ts(10));
        assert_eq!(q.pending()[0].reasons.len(), 2);
        // Re-enqueueing identical reasons does not duplicate.
        q.enqueue(&r2, ts(30));
        assert_eq!(q.pending()[0].reasons.len(), 2);
        assert!(q.resolve(rid(1)));
        assert!(!q.resolve(rid(1)));
        assert!(q.is_empty());
        records.clear();
    }
}
