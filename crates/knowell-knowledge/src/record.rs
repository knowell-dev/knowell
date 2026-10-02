//! The record state machine.
//!
//! Every method validates first and mutates only on success, so a failed
//! call leaves the record untouched. Every successful call appends a
//! [`HistoryEntry`] carrying actor, reason and timestamp.

use crate::error::KnowledgeError;
use crate::guard::check_no_secrets;
use crate::ids::{SymbolId, Timestamp};
use crate::model::{
    Action, Actor, Evidence, HistoryEntry, KnowledgeRecord, NewRecord, RecordKind, RecordState,
    RecordVersion,
};
use crate::policy::{AcceptancePolicy, Decision, Rights};

const MAX_TITLE: usize = 200;
const MAX_BODY: usize = 32 * 1024;
const MAX_REASON: usize = 1000;
const MAX_TAGS: usize = 32;
const MAX_TAG_LEN: usize = 64;
const MAX_EVIDENCE: usize = 64;
const MAX_SYMBOLS: usize = 256;

/// Changes for [`KnowledgeRecord::edit`]. Fields left `None` are kept.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EditPatch {
    /// New title.
    pub title: Option<String>,
    /// New Markdown body.
    pub body: Option<String>,
    /// New tags (replaces the old set).
    pub tags: Option<Vec<String>>,
    /// New evidence (replaces the old set). Required to leave the stale state.
    pub evidence: Option<Vec<Evidence>>,
    /// New related symbols (replaces the old set).
    pub related_symbols: Option<Vec<SymbolId>>,
}

/// Result of [`KnowledgeRecord::write`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    /// The new record, either proposed or accepted.
    pub record: KnowledgeRecord,
    /// What the policy decided.
    pub decision: Decision,
}

fn check_title(title: &str) -> Result<String, KnowledgeError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(KnowledgeError::EmptyField("title"));
    }
    if title.len() > MAX_TITLE {
        return Err(KnowledgeError::TooLong {
            field: "title",
            limit: MAX_TITLE,
        });
    }
    if title.contains(['\n', '\r']) {
        return Err(KnowledgeError::InvalidField {
            field: "title",
            reason: "must be a single line",
        });
    }
    check_no_secrets("title", title)?;
    Ok(title.to_string())
}

fn check_body(body: &str) -> Result<String, KnowledgeError> {
    if body.trim().is_empty() {
        return Err(KnowledgeError::EmptyField("body"));
    }
    if body.len() > MAX_BODY {
        return Err(KnowledgeError::TooLong {
            field: "body",
            limit: MAX_BODY,
        });
    }
    check_no_secrets("body", body)?;
    Ok(body.to_string())
}

fn check_tags(tags: Vec<String>) -> Result<Vec<String>, KnowledgeError> {
    let mut out = std::collections::BTreeSet::new();
    for tag in tags {
        let tag = tag.trim().to_lowercase();
        if tag.is_empty() {
            return Err(KnowledgeError::EmptyField("tag"));
        }
        if tag.len() > MAX_TAG_LEN {
            return Err(KnowledgeError::TooLong {
                field: "tag",
                limit: MAX_TAG_LEN,
            });
        }
        if tag.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(KnowledgeError::InvalidField {
                field: "tag",
                reason: "must not contain whitespace or control characters",
            });
        }
        check_no_secrets("tag", &tag)?;
        out.insert(tag);
    }
    if out.len() > MAX_TAGS {
        return Err(KnowledgeError::TooLong {
            field: "tags",
            limit: MAX_TAGS,
        });
    }
    Ok(out.into_iter().collect())
}

fn check_evidence(evidence: &[Evidence]) -> Result<(), KnowledgeError> {
    if evidence.len() > MAX_EVIDENCE {
        return Err(KnowledgeError::TooLong {
            field: "evidence",
            limit: MAX_EVIDENCE,
        });
    }
    Ok(())
}

fn check_symbols(symbols: Vec<SymbolId>) -> Result<Vec<SymbolId>, KnowledgeError> {
    let set: std::collections::BTreeSet<SymbolId> = symbols.into_iter().collect();
    if set.len() > MAX_SYMBOLS {
        return Err(KnowledgeError::TooLong {
            field: "related symbols",
            limit: MAX_SYMBOLS,
        });
    }
    Ok(set.into_iter().collect())
}

fn check_reason(reason: &str) -> Result<String, KnowledgeError> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(KnowledgeError::EmptyField("reason"));
    }
    if reason.len() > MAX_REASON {
        return Err(KnowledgeError::TooLong {
            field: "reason",
            limit: MAX_REASON,
        });
    }
    check_no_secrets("reason", reason)?;
    Ok(reason.to_string())
}

impl KnowledgeRecord {
    /// Creates a record in the [`RecordState::Proposed`] state.
    ///
    /// Validates every field and rejects secrets in title, body and tags.
    pub fn propose(new: NewRecord, author: Actor, at: Timestamp) -> Result<Self, KnowledgeError> {
        let title = check_title(&new.title)?;
        let body = check_body(&new.body)?;
        let tags = check_tags(new.tags)?;
        check_evidence(&new.evidence)?;
        if new.kind == RecordKind::Observed && new.evidence.is_empty() {
            return Err(KnowledgeError::MissingEvidence);
        }
        let related_symbols = check_symbols(new.related_symbols)?;
        Ok(Self {
            id: new.id,
            scope: new.scope,
            kind: new.kind,
            subject: new.subject,
            title,
            body,
            state: RecordState::Proposed,
            version: 1,
            created_at: at,
            updated_at: at,
            evidence: new.evidence,
            related_symbols,
            tags,
            pinned: new.pinned,
            superseded_by: None,
            previous_versions: Vec::new(),
            history: vec![HistoryEntry {
                at,
                actor: author.clone(),
                action: Action::Propose,
                from: None,
                to: RecordState::Proposed,
                version: 1,
                reason: "proposed".to_string(),
            }],
            author,
        })
    }

    /// Proposes a record and applies `policy`: if it decides to auto-accept,
    /// the record is accepted in the same call (history shows both steps).
    pub fn write(
        new: NewRecord,
        author: Actor,
        rights: Rights,
        policy: &AcceptancePolicy,
        at: Timestamp,
    ) -> Result<WriteOutcome, KnowledgeError> {
        let mut record = Self::propose(new, author.clone(), at)?;
        let decision = policy.decide(record.kind, &author, rights);
        if let Decision::AutoAccept { reason } = decision {
            record.push(
                at,
                &author,
                Action::Accept,
                RecordState::Accepted,
                format!("auto-accepted: {reason}"),
            );
        }
        Ok(WriteOutcome { record, decision })
    }

    fn push(
        &mut self,
        at: Timestamp,
        actor: &Actor,
        action: Action,
        to: RecordState,
        reason: String,
    ) {
        let from = self.state;
        self.state = to;
        self.updated_at = at;
        self.history.push(HistoryEntry {
            at,
            actor: actor.clone(),
            action,
            from: Some(from),
            to,
            version: self.version,
            reason,
        });
    }

    fn require_state(&self, action: Action, allowed: &[RecordState]) -> Result<(), KnowledgeError> {
        if allowed.contains(&self.state) {
            Ok(())
        } else {
            Err(KnowledgeError::IllegalTransition {
                action,
                from: self.state,
            })
        }
    }

    fn is_human_author(&self, actor: &Actor) -> bool {
        matches!((actor, &self.author), (Actor::Human(a), Actor::Human(b)) if a == b)
    }

    /// Accepts a **proposed** record.
    ///
    /// Allowed from `Proposed`. Stale records return through
    /// [`KnowledgeRecord::revalidate`], never through this method.
    pub fn accept(
        &mut self,
        actor: &Actor,
        rights: Rights,
        policy: &AcceptancePolicy,
        reason: &str,
        at: Timestamp,
    ) -> Result<(), KnowledgeError> {
        self.require_state(Action::Accept, &[RecordState::Proposed])?;
        if !policy.may_review(actor, rights, self.kind) {
            return Err(KnowledgeError::NotAuthorized {
                action: Action::Accept,
                reason: "only a human with the right to accept (or the engine, for observed records) may accept",
            });
        }
        let reason = check_reason(reason)?;
        self.push(at, actor, Action::Accept, RecordState::Accepted, reason);
        Ok(())
    }

    /// Rejects a proposed record, or retracts an accepted or stale one.
    ///
    /// Allowed from `Proposed`, `Accepted`, `Stale`. The actor must hold
    /// [`Rights::can_accept`], or be the human who authored the record
    /// (withdrawing their own work).
    pub fn reject(
        &mut self,
        actor: &Actor,
        rights: Rights,
        reason: &str,
        at: Timestamp,
    ) -> Result<(), KnowledgeError> {
        self.require_state(
            Action::Reject,
            &[
                RecordState::Proposed,
                RecordState::Accepted,
                RecordState::Stale,
            ],
        )?;
        let allowed = match actor {
            Actor::Human(_) => rights.can_accept || self.is_human_author(actor),
            Actor::System | Actor::Agent { .. } => false,
        };
        if !allowed {
            return Err(KnowledgeError::NotAuthorized {
                action: Action::Reject,
                reason: "only a human with the right to accept, or the human author, may reject",
            });
        }
        let reason = check_reason(reason)?;
        self.push(at, actor, Action::Reject, RecordState::Rejected, reason);
        Ok(())
    }

    /// Flags an accepted record as stale because its evidence changed.
    ///
    /// Allowed from `Accepted`. Only the engine or a human may flag; the
    /// record is kept and queued for revalidation, never deleted.
    pub fn mark_stale(
        &mut self,
        actor: &Actor,
        reason: &str,
        at: Timestamp,
    ) -> Result<(), KnowledgeError> {
        self.require_state(Action::MarkStale, &[RecordState::Accepted])?;
        if matches!(actor, Actor::Agent { .. }) {
            return Err(KnowledgeError::NotAuthorized {
                action: Action::MarkStale,
                reason: "agents report findings as suggestions; they cannot change record state",
            });
        }
        let reason = check_reason(reason)?;
        self.push(at, actor, Action::MarkStale, RecordState::Stale, reason);
        Ok(())
    }

    /// Confirms a stale record is still true, returning it to `Accepted`.
    ///
    /// Allowed from `Stale`. If `evidence` is given it replaces the old
    /// evidence (re-pointing it at the current code) and the version is bumped.
    pub fn revalidate(
        &mut self,
        actor: &Actor,
        rights: Rights,
        policy: &AcceptancePolicy,
        evidence: Option<Vec<Evidence>>,
        reason: &str,
        at: Timestamp,
    ) -> Result<(), KnowledgeError> {
        self.require_state(Action::Revalidate, &[RecordState::Stale])?;
        if !policy.may_review(actor, rights, self.kind) {
            return Err(KnowledgeError::NotAuthorized {
                action: Action::Revalidate,
                reason: "only a human with the right to accept (or the engine, for observed records) may revalidate",
            });
        }
        let reason = check_reason(reason)?;
        if let Some(ev) = &evidence {
            check_evidence(ev)?;
            if self.kind == RecordKind::Observed && ev.is_empty() {
                return Err(KnowledgeError::MissingEvidence);
            }
        }
        if let Some(ev) = evidence {
            self.archive_version(at);
            self.evidence = ev;
            self.version = self.version.saturating_add(1);
        }
        self.push(at, actor, Action::Revalidate, RecordState::Accepted, reason);
        Ok(())
    }

    /// Replaces this record by `by`.
    ///
    /// Allowed from `Accepted` and `Stale`. `by` must be a different,
    /// accepted record with the same subject and an overlapping scope. The
    /// actor must hold [`Rights::can_accept`].
    pub fn supersede(
        &mut self,
        by: &KnowledgeRecord,
        actor: &Actor,
        rights: Rights,
        reason: &str,
        at: Timestamp,
    ) -> Result<(), KnowledgeError> {
        self.require_state(
            Action::Supersede,
            &[RecordState::Accepted, RecordState::Stale],
        )?;
        if !matches!(actor, Actor::Human(_)) || !rights.can_accept {
            return Err(KnowledgeError::NotAuthorized {
                action: Action::Supersede,
                reason: "only a human with the right to accept may supersede a record",
            });
        }
        if by.id == self.id {
            return Err(KnowledgeError::InvalidSupersede(
                "a record cannot supersede itself",
            ));
        }
        if by.state != RecordState::Accepted {
            return Err(KnowledgeError::InvalidSupersede(
                "the replacement must be accepted",
            ));
        }
        if by.subject != self.subject {
            return Err(KnowledgeError::InvalidSupersede(
                "the replacement must have the same subject",
            ));
        }
        if !by.scope.overlaps(&self.scope) {
            return Err(KnowledgeError::InvalidSupersede(
                "the replacement must have an overlapping scope",
            ));
        }
        let reason = check_reason(reason)?;
        self.superseded_by = Some(by.id);
        self.push(
            at,
            actor,
            Action::Supersede,
            RecordState::Superseded,
            reason,
        );
        Ok(())
    }

    /// Creates a new version with changed content.
    ///
    /// Allowed from `Proposed`, `Accepted`, `Stale`; `expected_version` must
    /// match the current version. Who may edit: a human with
    /// [`Rights::can_accept`] or the human author; the engine for observed
    /// records; an agent only its own still-proposed record.
    ///
    /// The resulting state is decided by `policy`: when the edit would
    /// auto-accept it stays or becomes `Accepted`, otherwise it goes back to
    /// `Proposed` for review. A stale record without new evidence stays
    /// stale (text edits do not revalidate).
    #[allow(clippy::too_many_arguments)]
    pub fn edit(
        &mut self,
        expected_version: u32,
        patch: EditPatch,
        actor: &Actor,
        rights: Rights,
        policy: &AcceptancePolicy,
        reason: &str,
        at: Timestamp,
    ) -> Result<(), KnowledgeError> {
        self.require_state(
            Action::Edit,
            &[
                RecordState::Proposed,
                RecordState::Accepted,
                RecordState::Stale,
            ],
        )?;
        let allowed = match actor {
            Actor::Human(_) => rights.can_accept || self.is_human_author(actor),
            Actor::System => self.kind == RecordKind::Observed,
            Actor::Agent { .. } => self.state == RecordState::Proposed && &self.author == actor,
        };
        if !allowed {
            return Err(KnowledgeError::NotAuthorized {
                action: Action::Edit,
                reason: "you may edit only records you authored or review",
            });
        }
        if self.version != expected_version {
            return Err(KnowledgeError::VersionConflict {
                expected: expected_version,
                actual: self.version,
            });
        }
        if patch == EditPatch::default() {
            return Err(KnowledgeError::EmptyField("edit patch"));
        }
        let reason = check_reason(reason)?;
        let title = patch.title.as_deref().map(check_title).transpose()?;
        let body = patch.body.as_deref().map(check_body).transpose()?;
        let tags = patch.tags.map(check_tags).transpose()?;
        let symbols = patch.related_symbols.map(check_symbols).transpose()?;
        if let Some(ev) = &patch.evidence {
            check_evidence(ev)?;
        }
        let new_evidence_len = patch
            .evidence
            .as_ref()
            .map_or(self.evidence.len(), Vec::len);
        if self.kind == RecordKind::Observed && new_evidence_len == 0 {
            return Err(KnowledgeError::MissingEvidence);
        }

        let evidence_replaced = patch.evidence.is_some();
        let new_state = match (self.state, policy.decide(self.kind, actor, rights)) {
            (RecordState::Stale, _) if !evidence_replaced => RecordState::Stale,
            (_, Decision::AutoAccept { .. }) => RecordState::Accepted,
            _ => RecordState::Proposed,
        };

        self.archive_version(at);
        if let Some(t) = title {
            self.title = t;
        }
        if let Some(b) = body {
            self.body = b;
        }
        if let Some(t) = tags {
            self.tags = t;
        }
        if let Some(s) = symbols {
            self.related_symbols = s;
        }
        if let Some(e) = patch.evidence {
            self.evidence = e;
        }
        self.version = self.version.saturating_add(1);
        self.push(at, actor, Action::Edit, new_state, reason);
        Ok(())
    }

    /// Pins or unpins the record. Allowed in any non-terminal state; the
    /// actor must be a human holding [`Rights::can_accept`].
    pub fn set_pinned(
        &mut self,
        pinned: bool,
        actor: &Actor,
        rights: Rights,
        at: Timestamp,
    ) -> Result<(), KnowledgeError> {
        self.require_state(
            Action::Pin,
            &[
                RecordState::Proposed,
                RecordState::Accepted,
                RecordState::Stale,
            ],
        )?;
        if !matches!(actor, Actor::Human(_)) || !rights.can_accept {
            return Err(KnowledgeError::NotAuthorized {
                action: Action::Pin,
                reason: "only a human with the right to accept may pin",
            });
        }
        self.pinned = pinned;
        let state = self.state;
        let reason = if pinned { "pinned" } else { "unpinned" };
        self.push(at, actor, Action::Pin, state, reason.to_string());
        Ok(())
    }

    fn archive_version(&mut self, replaced_at: Timestamp) {
        self.previous_versions.push(RecordVersion {
            version: self.version,
            title: self.title.clone(),
            body: self.body.clone(),
            tags: self.tags.clone(),
            evidence: self.evidence.clone(),
            replaced_at,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    const POLICY: AcceptancePolicy = AcceptancePolicy {
        auto_accept_observed: true,
        auto_accept_human: true,
    };

    fn proposed(kind: RecordKind, n: u128) -> KnowledgeRecord {
        let author = if kind == RecordKind::Observed {
            Actor::System
        } else {
            human("author")
        };
        KnowledgeRecord::propose(new_record(kind, n), author, ts(1)).unwrap()
    }

    fn accepted(kind: RecordKind, n: u128) -> KnowledgeRecord {
        let mut r = proposed(kind, n);
        r.accept(&human("rev"), Rights::REVIEWER, &POLICY, "ok", ts(2))
            .unwrap();
        r
    }

    fn stale(n: u128) -> KnowledgeRecord {
        let mut r = accepted(RecordKind::Human, n);
        r.mark_stale(&Actor::System, "evidence changed", ts(3))
            .unwrap();
        r
    }

    fn rejected(n: u128) -> KnowledgeRecord {
        let mut r = proposed(RecordKind::Human, n);
        r.reject(&human("rev"), Rights::REVIEWER, "no", ts(2))
            .unwrap();
        r
    }

    fn superseded(n: u128) -> KnowledgeRecord {
        let mut r = accepted(RecordKind::Human, n);
        let by = accepted(RecordKind::Human, n + 100);
        r.supersede(&by, &human("rev"), Rights::REVIEWER, "newer", ts(3))
            .unwrap();
        r
    }

    /// One record in every state, to drive the illegal-transition matrix.
    fn in_state(state: RecordState) -> KnowledgeRecord {
        match state {
            RecordState::Proposed => proposed(RecordKind::Human, 1),
            RecordState::Accepted => accepted(RecordKind::Human, 1),
            RecordState::Stale => stale(1),
            RecordState::Rejected => rejected(1),
            RecordState::Superseded => superseded(1),
        }
    }

    const ALL: [RecordState; 5] = [
        RecordState::Proposed,
        RecordState::Accepted,
        RecordState::Stale,
        RecordState::Rejected,
        RecordState::Superseded,
    ];

    fn is_illegal(r: Result<(), KnowledgeError>) -> bool {
        matches!(r, Err(KnowledgeError::IllegalTransition { .. }))
    }

    #[test]
    fn propose_creates_version_one_with_history() {
        let r = proposed(RecordKind::Human, 1);
        assert_eq!(r.state, RecordState::Proposed);
        assert_eq!(r.version, 1);
        assert_eq!(r.history.len(), 1);
        assert_eq!(r.history[0].action, Action::Propose);
        assert_eq!(r.history[0].from, None);
        assert!(!r.is_current());
    }

    #[test]
    fn propose_validates_fields() {
        let mut n = new_record(RecordKind::Human, 1);
        n.title = "  ".into();
        assert_eq!(
            KnowledgeRecord::propose(n, human("a"), ts(1)),
            Err(KnowledgeError::EmptyField("title"))
        );
        let mut n = new_record(RecordKind::Human, 1);
        n.title = "a\nb".into();
        assert!(matches!(
            KnowledgeRecord::propose(n, human("a"), ts(1)),
            Err(KnowledgeError::InvalidField { .. })
        ));
        let mut n = new_record(RecordKind::Human, 1);
        n.body = String::new();
        assert_eq!(
            KnowledgeRecord::propose(n, human("a"), ts(1)),
            Err(KnowledgeError::EmptyField("body"))
        );
        let mut n = new_record(RecordKind::Human, 1);
        n.body = "x".repeat(MAX_BODY + 1);
        assert!(matches!(
            KnowledgeRecord::propose(n, human("a"), ts(1)),
            Err(KnowledgeError::TooLong { .. })
        ));
        let mut n = new_record(RecordKind::Observed, 1);
        n.evidence.clear();
        assert_eq!(
            KnowledgeRecord::propose(n, Actor::System, ts(1)),
            Err(KnowledgeError::MissingEvidence)
        );
        let mut n = new_record(RecordKind::Human, 1);
        n.tags = vec!["has space".into()];
        assert!(KnowledgeRecord::propose(n, human("a"), ts(1)).is_err());
    }

    #[test]
    fn propose_normalises_tags_and_symbols() {
        let mut n = new_record(RecordKind::Human, 1);
        n.tags = vec!["Rule".into(), "rule".into(), " API ".into()];
        n.related_symbols = vec![sym("b"), sym("a"), sym("b")];
        let r = KnowledgeRecord::propose(n, human("a"), ts(1)).unwrap();
        assert_eq!(r.tags, vec!["api", "rule"]);
        assert_eq!(r.related_symbols, vec![sym("a"), sym("b")]);
        assert!(r.is_rule());
    }

    #[test]
    fn propose_rejects_secrets_everywhere() {
        let token = fake_token();
        let mut n = new_record(RecordKind::Human, 1);
        n.body = format!("line one\nkey {token}");
        let err = KnowledgeRecord::propose(n, human("a"), ts(1)).unwrap_err();
        assert_eq!(
            err,
            KnowledgeError::SecretDetected {
                field: "body",
                kind: "github_token",
                line: 2
            }
        );
        assert!(!err.to_string().contains(&token));
        let mut n = new_record(RecordKind::Human, 1);
        n.title = format!("t {token}");
        assert!(matches!(
            KnowledgeRecord::propose(n, human("a"), ts(1)),
            Err(KnowledgeError::SecretDetected { field: "title", .. })
        ));
        let mut n = new_record(RecordKind::Human, 1);
        n.tags = vec![token.to_lowercase()];
        assert!(KnowledgeRecord::propose(n, human("a"), ts(1)).is_err());
    }

    #[test]
    fn write_auto_accepts_per_policy() {
        let out = KnowledgeRecord::write(
            new_record(RecordKind::Observed, 1),
            Actor::System,
            Rights::NONE,
            &POLICY,
            ts(1),
        )
        .unwrap();
        assert_eq!(out.record.state, RecordState::Accepted);
        assert_eq!(out.record.history.len(), 2);
        assert!(matches!(out.decision, Decision::AutoAccept { .. }));

        let out = KnowledgeRecord::write(
            new_record(RecordKind::Human, 2),
            human("a"),
            Rights::REVIEWER,
            &POLICY,
            ts(1),
        )
        .unwrap();
        assert_eq!(out.record.state, RecordState::Accepted);

        let out = KnowledgeRecord::write(
            new_record(RecordKind::Human, 3),
            human("a"),
            Rights::NONE,
            &POLICY,
            ts(1),
        )
        .unwrap();
        assert_eq!(out.record.state, RecordState::Proposed);

        let out = KnowledgeRecord::write(
            new_record(RecordKind::ModelSuggestion, 4),
            agent("s"),
            Rights::REVIEWER,
            &POLICY,
            ts(1),
        )
        .unwrap();
        assert_eq!(out.record.state, RecordState::Proposed);
        assert!(matches!(out.decision, Decision::NeedsReview { .. }));
    }

    #[test]
    fn write_rejects_secret_bodies() {
        let mut n = new_record(RecordKind::Human, 1);
        n.body = format!("token={}", fake_token());
        assert!(matches!(
            KnowledgeRecord::write(n, human("a"), Rights::REVIEWER, &POLICY, ts(1)),
            Err(KnowledgeError::SecretDetected { .. })
        ));
    }

    #[test]
    fn accept_legal_and_records_history() {
        let mut r = proposed(RecordKind::Human, 1);
        r.accept(
            &human("rev"),
            Rights::REVIEWER,
            &POLICY,
            "looks right",
            ts(5),
        )
        .unwrap();
        assert_eq!(r.state, RecordState::Accepted);
        assert_eq!(r.updated_at, ts(5));
        let last = r.history.last().unwrap();
        assert_eq!(
            (last.action, last.from, last.to),
            (
                Action::Accept,
                Some(RecordState::Proposed),
                RecordState::Accepted
            )
        );
        assert_eq!(last.reason, "looks right");
        assert_eq!(last.actor, human("rev"));
        assert!(r.is_current());
    }

    #[test]
    fn accept_illegal_from_every_other_state() {
        for s in ALL.into_iter().filter(|s| *s != RecordState::Proposed) {
            let mut r = in_state(s);
            let before = r.clone();
            assert!(
                is_illegal(r.accept(&human("rev"), Rights::REVIEWER, &POLICY, "x", ts(9))),
                "{s}"
            );
            assert_eq!(r, before, "failed call must not mutate");
        }
    }

    #[test]
    fn accept_requires_authority() {
        let mut r = proposed(RecordKind::ModelSuggestion, 1);
        for (actor, rights) in [
            (agent("s"), Rights::REVIEWER),
            (human("x"), Rights::NONE),
            (Actor::System, Rights::REVIEWER),
        ] {
            assert!(matches!(
                r.accept(&actor, rights, &POLICY, "x", ts(2)),
                Err(KnowledgeError::NotAuthorized {
                    action: Action::Accept,
                    ..
                })
            ));
        }
        assert_eq!(r.state, RecordState::Proposed);
    }

    #[test]
    fn accept_requires_a_reason() {
        let mut r = proposed(RecordKind::Human, 1);
        assert_eq!(
            r.accept(&human("rev"), Rights::REVIEWER, &POLICY, "  ", ts(2)),
            Err(KnowledgeError::EmptyField("reason"))
        );
    }

    #[test]
    fn reason_is_secret_checked() {
        let mut r = proposed(RecordKind::Human, 1);
        let reason = format!("because {}", fake_token());
        assert!(matches!(
            r.accept(&human("rev"), Rights::REVIEWER, &POLICY, &reason, ts(2)),
            Err(KnowledgeError::SecretDetected {
                field: "reason",
                ..
            })
        ));
    }

    #[test]
    fn reject_legal_from_proposed_accepted_stale() {
        for s in [
            RecordState::Proposed,
            RecordState::Accepted,
            RecordState::Stale,
        ] {
            let mut r = in_state(s);
            r.reject(&human("rev"), Rights::REVIEWER, "wrong", ts(9))
                .unwrap();
            assert_eq!(r.state, RecordState::Rejected, "{s}");
            assert_eq!(r.history.last().unwrap().from, Some(s));
        }
    }

    #[test]
    fn reject_illegal_from_terminal_states() {
        for s in [RecordState::Rejected, RecordState::Superseded] {
            let mut r = in_state(s);
            assert!(
                is_illegal(r.reject(&human("rev"), Rights::REVIEWER, "x", ts(9))),
                "{s}"
            );
        }
    }

    #[test]
    fn reject_authority() {
        let mut r = proposed(RecordKind::Human, 1);
        assert!(r.reject(&agent("s"), Rights::REVIEWER, "x", ts(2)).is_err());
        assert!(
            r.reject(&Actor::System, Rights::REVIEWER, "x", ts(2))
                .is_err()
        );
        assert!(
            r.reject(&human("someone"), Rights::NONE, "x", ts(2))
                .is_err()
        );
        // The human author may withdraw their own proposal.
        r.reject(&human("author"), Rights::NONE, "withdrawn", ts(3))
            .unwrap();
        assert_eq!(r.state, RecordState::Rejected);
    }

    #[test]
    fn mark_stale_legal_only_from_accepted() {
        let mut r = accepted(RecordKind::Human, 1);
        r.mark_stale(&Actor::System, "file changed", ts(4)).unwrap();
        assert_eq!(r.state, RecordState::Stale);
        assert!(!r.is_current());
        for s in ALL.into_iter().filter(|s| *s != RecordState::Accepted) {
            let mut r = in_state(s);
            assert!(is_illegal(r.mark_stale(&Actor::System, "x", ts(9))), "{s}");
        }
    }

    #[test]
    fn mark_stale_agents_cannot() {
        let mut r = accepted(RecordKind::Human, 1);
        assert!(matches!(
            r.mark_stale(&agent("s"), "x", ts(4)),
            Err(KnowledgeError::NotAuthorized {
                action: Action::MarkStale,
                ..
            })
        ));
        r.mark_stale(&human("any"), "doubt", ts(4)).unwrap();
    }

    #[test]
    fn revalidate_legal_from_stale() {
        let mut r = stale(1);
        r.revalidate(
            &human("rev"),
            Rights::REVIEWER,
            &POLICY,
            None,
            "still true",
            ts(6),
        )
        .unwrap();
        assert_eq!(r.state, RecordState::Accepted);
        assert_eq!(r.version, 1);
        assert_eq!(r.history.last().unwrap().action, Action::Revalidate);
    }

    #[test]
    fn revalidate_with_new_evidence_bumps_version() {
        let mut r = stale(1);
        let ev = vec![evidence("src/new.rs", "new")];
        r.revalidate(
            &human("rev"),
            Rights::REVIEWER,
            &POLICY,
            Some(ev.clone()),
            "re-pointed",
            ts(6),
        )
        .unwrap();
        assert_eq!(r.version, 2);
        assert_eq!(r.evidence, ev);
        assert_eq!(r.previous_versions.len(), 1);
        assert_eq!(r.previous_versions[0].version, 1);
    }

    #[test]
    fn revalidate_illegal_from_other_states() {
        for s in ALL.into_iter().filter(|s| *s != RecordState::Stale) {
            let mut r = in_state(s);
            assert!(
                is_illegal(r.revalidate(
                    &human("rev"),
                    Rights::REVIEWER,
                    &POLICY,
                    None,
                    "x",
                    ts(9)
                )),
                "{s}"
            );
        }
    }

    #[test]
    fn revalidate_authority() {
        let mut r = stale(1);
        assert!(
            r.revalidate(&agent("s"), Rights::REVIEWER, &POLICY, None, "x", ts(6))
                .is_err()
        );
        assert!(
            r.revalidate(&human("x"), Rights::NONE, &POLICY, None, "x", ts(6))
                .is_err()
        );
        assert!(
            r.revalidate(&Actor::System, Rights::NONE, &POLICY, None, "x", ts(6))
                .is_err(),
            "system may not revalidate human records"
        );
        let mut obs = accepted(RecordKind::Observed, 2);
        obs.mark_stale(&Actor::System, "changed", ts(3)).unwrap();
        obs.revalidate(
            &Actor::System,
            Rights::NONE,
            &POLICY,
            Some(vec![evidence("a.rs", "h2")]),
            "re-extracted",
            ts(4),
        )
        .unwrap();
        assert_eq!(obs.state, RecordState::Accepted);
    }

    #[test]
    fn supersede_legal() {
        let mut r = accepted(RecordKind::Human, 1);
        let by = accepted(RecordKind::Human, 2);
        r.supersede(&by, &human("rev"), Rights::REVIEWER, "replaced", ts(7))
            .unwrap();
        assert_eq!(r.state, RecordState::Superseded);
        assert_eq!(r.superseded_by, Some(by.id));
        let mut s = stale(3);
        s.supersede(&by, &human("rev"), Rights::REVIEWER, "replaced", ts(7))
            .unwrap();
        assert_eq!(s.state, RecordState::Superseded);
    }

    #[test]
    fn supersede_illegal_states_and_bad_replacements() {
        let by = accepted(RecordKind::Human, 2);
        for s in [
            RecordState::Proposed,
            RecordState::Rejected,
            RecordState::Superseded,
        ] {
            let mut r = in_state(s);
            assert!(
                is_illegal(r.supersede(&by, &human("rev"), Rights::REVIEWER, "x", ts(9))),
                "{s}"
            );
        }
        let mut r = accepted(RecordKind::Human, 1);
        let itself = r.clone();
        assert!(matches!(
            r.supersede(&itself, &human("rev"), Rights::REVIEWER, "x", ts(9)),
            Err(KnowledgeError::InvalidSupersede(_))
        ));
        let not_accepted = proposed(RecordKind::Human, 5);
        assert!(matches!(
            r.supersede(&not_accepted, &human("rev"), Rights::REVIEWER, "x", ts(9)),
            Err(KnowledgeError::InvalidSupersede(_))
        ));
        let mut other_subject = accepted(RecordKind::Human, 6);
        other_subject.subject = crate::ids::Subject::new("other.thing").unwrap();
        assert!(matches!(
            r.supersede(&other_subject, &human("rev"), Rights::REVIEWER, "x", ts(9)),
            Err(KnowledgeError::InvalidSupersede(_))
        ));
        let mut other_scope = accepted(RecordKind::Human, 7);
        other_scope.scope = crate::model::Scope::User(crate::ids::UserId::new("zed").unwrap());
        assert!(matches!(
            r.supersede(&other_scope, &human("rev"), Rights::REVIEWER, "x", ts(9)),
            Err(KnowledgeError::InvalidSupersede(_))
        ));
        assert!(matches!(
            r.supersede(&by, &agent("s"), Rights::REVIEWER, "x", ts(9)),
            Err(KnowledgeError::NotAuthorized { .. })
        ));
        assert_eq!(r.state, RecordState::Accepted);
    }

    #[test]
    fn edit_by_reviewer_keeps_accepted_and_bumps_version() {
        let mut r = accepted(RecordKind::Human, 1);
        let patch = EditPatch {
            body: Some("new body".into()),
            ..EditPatch::default()
        };
        r.edit(
            1,
            patch,
            &human("rev"),
            Rights::REVIEWER,
            &POLICY,
            "clarify",
            ts(8),
        )
        .unwrap();
        assert_eq!(r.version, 2);
        assert_eq!(r.body, "new body");
        assert_eq!(r.state, RecordState::Accepted);
        assert_eq!(r.previous_versions.len(), 1);
        assert_eq!(r.previous_versions[0].body, "Body text for the record.");
        assert_eq!(r.history.last().unwrap().action, Action::Edit);
        assert_eq!(r.history.last().unwrap().version, 2);
    }

    #[test]
    fn edit_by_author_without_rights_returns_to_review() {
        let mut r = accepted(RecordKind::Human, 1);
        let patch = EditPatch {
            title: Some("Retitled".into()),
            ..EditPatch::default()
        };
        r.edit(
            1,
            patch,
            &human("author"),
            Rights::NONE,
            &POLICY,
            "retitle",
            ts(8),
        )
        .unwrap();
        assert_eq!(r.state, RecordState::Proposed);
        assert_eq!(r.history.last().unwrap().from, Some(RecordState::Accepted));
    }

    #[test]
    fn edit_stale_needs_new_evidence_to_revive() {
        let mut r = stale(1);
        r.edit(
            1,
            EditPatch {
                body: Some("b".into()),
                ..EditPatch::default()
            },
            &human("rev"),
            Rights::REVIEWER,
            &POLICY,
            "text",
            ts(8),
        )
        .unwrap();
        assert_eq!(r.state, RecordState::Stale);
        r.edit(
            2,
            EditPatch {
                evidence: Some(vec![evidence("a.rs", "h9")]),
                ..EditPatch::default()
            },
            &human("rev"),
            Rights::REVIEWER,
            &POLICY,
            "re-pointed",
            ts(9),
        )
        .unwrap();
        assert_eq!(r.state, RecordState::Accepted);
    }

    #[test]
    fn edit_illegal_from_terminal_states() {
        for s in [RecordState::Rejected, RecordState::Superseded] {
            let mut r = in_state(s);
            let patch = EditPatch {
                body: Some("x".into()),
                ..EditPatch::default()
            };
            assert!(
                is_illegal(r.edit(
                    r.version,
                    patch,
                    &human("rev"),
                    Rights::REVIEWER,
                    &POLICY,
                    "x",
                    ts(9)
                )),
                "{s}"
            );
        }
    }

    #[test]
    fn edit_checks_version_authority_and_content() {
        let mut r = accepted(RecordKind::Human, 1);
        let patch = || EditPatch {
            body: Some("x".into()),
            ..EditPatch::default()
        };
        assert_eq!(
            r.edit(
                5,
                patch(),
                &human("rev"),
                Rights::REVIEWER,
                &POLICY,
                "x",
                ts(9)
            ),
            Err(KnowledgeError::VersionConflict {
                expected: 5,
                actual: 1
            })
        );
        assert!(matches!(
            r.edit(
                1,
                patch(),
                &agent("s"),
                Rights::REVIEWER,
                &POLICY,
                "x",
                ts(9)
            ),
            Err(KnowledgeError::NotAuthorized {
                action: Action::Edit,
                ..
            })
        ));
        assert!(matches!(
            r.edit(
                1,
                patch(),
                &human("stranger"),
                Rights::NONE,
                &POLICY,
                "x",
                ts(9)
            ),
            Err(KnowledgeError::NotAuthorized { .. })
        ));
        assert_eq!(
            r.edit(
                1,
                EditPatch::default(),
                &human("rev"),
                Rights::REVIEWER,
                &POLICY,
                "x",
                ts(9)
            ),
            Err(KnowledgeError::EmptyField("edit patch"))
        );
        let leaked = EditPatch {
            body: Some(format!("k {}", fake_token())),
            ..EditPatch::default()
        };
        assert!(matches!(
            r.edit(
                1,
                leaked,
                &human("rev"),
                Rights::REVIEWER,
                &POLICY,
                "x",
                ts(9)
            ),
            Err(KnowledgeError::SecretDetected { .. })
        ));
        assert_eq!(r.version, 1);
    }

    #[test]
    fn agent_may_edit_only_its_own_proposal() {
        let mut r = KnowledgeRecord::propose(
            new_record(RecordKind::ModelSuggestion, 1),
            agent("s1"),
            ts(1),
        )
        .unwrap();
        let patch = || EditPatch {
            body: Some("refined".into()),
            ..EditPatch::default()
        };
        r.edit(
            1,
            patch(),
            &agent("s1"),
            Rights::NONE,
            &POLICY,
            "refine",
            ts(2),
        )
        .unwrap();
        assert_eq!(r.state, RecordState::Proposed);
        assert!(
            r.edit(2, patch(), &agent("s2"), Rights::NONE, &POLICY, "x", ts(3))
                .is_err()
        );
    }

    #[test]
    fn observed_edit_cannot_drop_all_evidence() {
        let mut r = accepted(RecordKind::Observed, 1);
        let patch = EditPatch {
            evidence: Some(Vec::new()),
            ..EditPatch::default()
        };
        assert_eq!(
            r.edit(1, patch, &Actor::System, Rights::NONE, &POLICY, "x", ts(9)),
            Err(KnowledgeError::MissingEvidence)
        );
    }

    #[test]
    fn pin_and_unpin() {
        let mut r = accepted(RecordKind::Human, 1);
        r.set_pinned(true, &human("rev"), Rights::REVIEWER, ts(5))
            .unwrap();
        assert!(r.pinned);
        assert_eq!(r.state, RecordState::Accepted);
        r.set_pinned(false, &human("rev"), Rights::REVIEWER, ts(6))
            .unwrap();
        assert!(!r.pinned);
        assert!(
            r.set_pinned(true, &agent("s"), Rights::REVIEWER, ts(7))
                .is_err()
        );
        assert!(
            r.set_pinned(true, &human("x"), Rights::NONE, ts(7))
                .is_err()
        );
        for s in [RecordState::Rejected, RecordState::Superseded] {
            let mut t = in_state(s);
            assert!(
                is_illegal(t.set_pinned(true, &human("rev"), Rights::REVIEWER, ts(9))),
                "{s}"
            );
        }
    }

    #[test]
    fn history_is_append_only_and_ordered() {
        let mut r = proposed(RecordKind::Human, 1);
        r.accept(&human("rev"), Rights::REVIEWER, &POLICY, "ok", ts(2))
            .unwrap();
        r.mark_stale(&Actor::System, "changed", ts(3)).unwrap();
        r.revalidate(
            &human("rev"),
            Rights::REVIEWER,
            &POLICY,
            None,
            "fine",
            ts(4),
        )
        .unwrap();
        let actions: Vec<Action> = r.history.iter().map(|h| h.action).collect();
        assert_eq!(
            actions,
            vec![
                Action::Propose,
                Action::Accept,
                Action::MarkStale,
                Action::Revalidate
            ]
        );
        let times: Vec<i64> = r.history.iter().map(|h| h.at.unix_seconds()).collect();
        assert_eq!(times, vec![1, 2, 3, 4]);
    }

    #[test]
    fn record_serde_round_trip() {
        let r = accepted(RecordKind::Observed, 1);
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<KnowledgeRecord>(&json).unwrap(), r);
    }
}
