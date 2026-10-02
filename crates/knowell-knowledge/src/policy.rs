//! Who may accept what, and what is accepted automatically.

use serde::{Deserialize, Serialize};

use crate::model::{Actor, RecordKind};

/// Rights an actor holds in the scope of a record, decided by the caller's
/// permission system (this crate does not know about users or roles).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rights {
    /// May accept, reject, supersede and pin records of the scope.
    pub can_accept: bool,
}

impl Rights {
    /// No review rights.
    pub const NONE: Rights = Rights { can_accept: false };
    /// Full review rights.
    pub const REVIEWER: Rights = Rights { can_accept: true };
}

/// Outcome of [`AcceptancePolicy::decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// The record is accepted on write; the reason is recorded in its history.
    AutoAccept {
        /// Why the policy accepted it.
        reason: &'static str,
    },
    /// The record waits in the proposal queue.
    NeedsReview {
        /// Why it was not accepted automatically.
        reason: &'static str,
    },
}

/// Decides what auto-accepts. The defaults encode the product rules:
///
/// * observed records written by the engine itself (deterministic analysis)
///   auto-accept;
/// * human-authored records auto-accept when the author holds
///   [`Rights::can_accept`];
/// * agent suggestions never auto-accept, and an agent can never accept,
///   reject or revalidate anything, whatever rights it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptancePolicy {
    /// Auto-accept [`RecordKind::Observed`] records authored by [`Actor::System`].
    pub auto_accept_observed: bool,
    /// Auto-accept [`RecordKind::Human`] records authored by a human with rights.
    pub auto_accept_human: bool,
}

impl Default for AcceptancePolicy {
    fn default() -> Self {
        Self {
            auto_accept_observed: true,
            auto_accept_human: true,
        }
    }
}

impl AcceptancePolicy {
    /// Decides whether a freshly written record is accepted immediately.
    pub fn decide(&self, kind: RecordKind, author: &Actor, rights: Rights) -> Decision {
        match (kind, author) {
            (RecordKind::ModelSuggestion, _) => Decision::NeedsReview {
                reason: "agent suggestions are never accepted automatically",
            },
            (_, Actor::Agent { .. }) => Decision::NeedsReview {
                reason: "records written by an agent need human review",
            },
            (RecordKind::Observed, Actor::System) if self.auto_accept_observed => {
                Decision::AutoAccept {
                    reason: "observed from code by deterministic analysis",
                }
            }
            (RecordKind::Human, Actor::Human(_)) if self.auto_accept_human && rights.can_accept => {
                Decision::AutoAccept {
                    reason: "written by a human with the right to accept",
                }
            }
            _ => Decision::NeedsReview {
                reason: "needs review by a human with the right to accept",
            },
        }
    }

    /// Whether `actor` may accept or revalidate a record of `kind`.
    pub fn may_review(&self, actor: &Actor, rights: Rights, kind: RecordKind) -> bool {
        match actor {
            Actor::Human(_) => rights.can_accept,
            Actor::System => kind == RecordKind::Observed && self.auto_accept_observed,
            Actor::Agent { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ClientId, SessionId, UserId};

    fn human() -> Actor {
        Actor::Human(UserId::new("u1").unwrap())
    }
    fn agent() -> Actor {
        Actor::Agent {
            session: SessionId::new("s1").unwrap(),
            client: ClientId::new("c").unwrap(),
        }
    }

    #[test]
    fn observed_by_system_auto_accepts() {
        let p = AcceptancePolicy::default();
        assert!(matches!(
            p.decide(RecordKind::Observed, &Actor::System, Rights::NONE),
            Decision::AutoAccept { .. }
        ));
        let off = AcceptancePolicy {
            auto_accept_observed: false,
            ..p
        };
        assert!(matches!(
            off.decide(RecordKind::Observed, &Actor::System, Rights::NONE),
            Decision::NeedsReview { .. }
        ));
    }

    #[test]
    fn human_needs_rights_to_auto_accept() {
        let p = AcceptancePolicy::default();
        assert!(matches!(
            p.decide(RecordKind::Human, &human(), Rights::REVIEWER),
            Decision::AutoAccept { .. }
        ));
        assert!(matches!(
            p.decide(RecordKind::Human, &human(), Rights::NONE),
            Decision::NeedsReview { .. }
        ));
    }

    #[test]
    fn agents_never_auto_accept() {
        let p = AcceptancePolicy::default();
        for kind in [
            RecordKind::Observed,
            RecordKind::Human,
            RecordKind::ModelSuggestion,
        ] {
            assert!(matches!(
                p.decide(kind, &agent(), Rights::REVIEWER),
                Decision::NeedsReview { .. }
            ));
        }
        assert!(matches!(
            p.decide(RecordKind::ModelSuggestion, &human(), Rights::REVIEWER),
            Decision::NeedsReview { .. }
        ));
        assert!(matches!(
            p.decide(
                RecordKind::ModelSuggestion,
                &Actor::System,
                Rights::REVIEWER
            ),
            Decision::NeedsReview { .. }
        ));
    }

    #[test]
    fn review_rights() {
        let p = AcceptancePolicy::default();
        assert!(p.may_review(&human(), Rights::REVIEWER, RecordKind::ModelSuggestion));
        assert!(!p.may_review(&human(), Rights::NONE, RecordKind::Human));
        assert!(!p.may_review(&agent(), Rights::REVIEWER, RecordKind::Observed));
        assert!(p.may_review(&Actor::System, Rights::NONE, RecordKind::Observed));
        assert!(!p.may_review(&Actor::System, Rights::NONE, RecordKind::Human));
    }
}
