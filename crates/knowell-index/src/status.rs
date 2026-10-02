//! Freshness reporting: per-view status with the state of every tier, and
//! the progress events the HTTP server streams (SSE).

use std::collections::BTreeMap;
use std::time::Duration;

use knowell_core::{Name, TrackTarget};
use knowell_store::{ProfileId, ViewId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A freshness tier (ARCHITECTURE §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// File text and paths searchable (Tantivy).
    T0,
    /// Symbols, imports, chunks, syntactic edges.
    T1,
    /// Embeddings.
    T2,
    /// Relations (contract linking) and knowledge staleness.
    T3,
}

impl Tier {
    /// All tiers in pipeline order.
    pub const ALL: [Tier; 4] = [Tier::T0, Tier::T1, Tier::T2, Tier::T3];

    /// Position in [`Tier::ALL`].
    pub fn index(self) -> usize {
        match self {
            Tier::T0 => 0,
            Tier::T1 => 1,
            Tier::T2 => 2,
            Tier::T3 => 3,
        }
    }

    /// Lowercase name (`t0` .. `t3`).
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::T0 => "t0",
            Tier::T1 => "t1",
            Tier::T2 => "t2",
            Tier::T3 => "t3",
        }
    }
}

/// Why a tier was skipped. Serialized as the stable snake-case reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierSkip {
    /// No embedding provider is configured for the project; lexical, symbol
    /// and graph search still work.
    NoProvider,
    /// The project is `local-only` and its provider is a cloud service:
    /// nothing is sent.
    DataPolicyLocalOnly,
    /// The embedding budget does not allow this build's inputs.
    BudgetExhausted,
}

impl TierSkip {
    /// Stable reason text (`no_provider`, `data_policy_local_only`,
    /// `budget_exhausted`).
    pub fn as_str(self) -> &'static str {
        match self {
            TierSkip::NoProvider => "no_provider",
            TierSkip::DataPolicyLocalOnly => "data_policy_local_only",
            TierSkip::BudgetExhausted => "budget_exhausted",
        }
    }
}

/// State of one tier of the build a status describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TierState {
    /// Not started yet.
    Pending,
    /// In progress.
    Running,
    /// Finished.
    Done,
    /// Deliberately not done, with the reason.
    Skipped {
        /// Why.
        reason: TierSkip,
    },
    /// Failed; the message names the cause (never secret values).
    Failed {
        /// Why.
        reason: String,
    },
}

impl TierState {
    /// Whether the tier has reached a final state (done, skipped, failed).
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            TierState::Done | TierState::Skipped { .. } | TierState::Failed { .. }
        )
    }
}

/// The four tier states of a build, in tier order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierStates {
    /// Text and paths.
    pub t0: TierState,
    /// Symbols and syntactic edges.
    pub t1: TierState,
    /// Embeddings.
    pub t2: TierState,
    /// Relations and staleness.
    pub t3: TierState,
}

impl TierStates {
    /// Every tier pending.
    pub fn pending() -> Self {
        Self {
            t0: TierState::Pending,
            t1: TierState::Pending,
            t2: TierState::Pending,
            t3: TierState::Pending,
        }
    }

    /// The state of `tier`.
    pub fn get(&self, tier: Tier) -> &TierState {
        match tier {
            Tier::T0 => &self.t0,
            Tier::T1 => &self.t1,
            Tier::T2 => &self.t2,
            Tier::T3 => &self.t3,
        }
    }

    pub(crate) fn set(&mut self, tier: Tier, state: TierState) {
        match tier {
            Tier::T0 => self.t0 = state,
            Tier::T1 => self.t1 = state,
            Tier::T2 => self.t2 = state,
            Tier::T3 => self.t3 = state,
        }
    }
}

/// Freshness of one view: what it follows, what was seen, what is served,
/// what is being built, and how far behind it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ViewStatus {
    /// The view.
    pub view: ViewId,
    /// Workspace name.
    pub workspace: Name,
    /// Project name.
    pub project: Name,
    /// What the view follows (`branch:main`, `worktree`, ...).
    pub target: TrackTarget,
    /// Newest commit seen on the target.
    pub latest_seen_commit: Option<String>,
    /// Commit of the generation queries use (`None` for directory sources
    /// and views never activated).
    pub active_commit: Option<String>,
    /// Generation queries use.
    pub active_generation: Option<i64>,
    /// Generation being built, if any.
    pub building_generation: Option<i64>,
    /// Tier states of the build in progress, or of the active generation
    /// when nothing is building.
    pub tiers: TierStates,
    /// How long the view has been behind its target (`None` when the active
    /// generation is current).
    #[serde(with = "duration_secs")]
    pub lag: Option<Duration>,
    /// The most recent failure (missing ref, failed build), if it is newer
    /// than the active generation.
    pub last_error: Option<String>,
}

mod duration_secs {
    use std::time::Duration;

    use serde::Serializer;

    pub(super) fn serialize<S: Serializer>(
        value: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(d) => serializer.serialize_some(&d.as_secs_f64()),
            None => serializer.serialize_none(),
        }
    }
}

/// Embedding coverage of a view's active generation in its profile, from
/// [`crate::Indexer::embedding_coverage`].
///
/// T2 runs after activation, so a freshly activated generation is searchable
/// lexically, by symbol and through the graph while its vectors are still
/// being written. Queries use this to report "semantic coverage partial"
/// rather than present older vectors as current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EmbeddingCoverage {
    /// The view.
    pub view: ViewId,
    /// Its active generation.
    pub generation: i64,
    /// The embedding profile.
    pub profile: ProfileId,
    /// Chunks of the generation's files meant to be embedded (counted per
    /// path: identical content at two paths counts twice).
    pub inputs: u64,
    /// Of those, chunks whose prepared input has a vector in the profile.
    pub embedded: u64,
    /// Whether T2 finished for this generation (its vector index generation
    /// is the active one). Inputs the provider refused as too long keep
    /// `embedded` below `inputs` even then.
    pub complete: bool,
}

impl EmbeddingCoverage {
    /// Whether some inputs have no vector yet (or will never have one).
    pub fn is_partial(&self) -> bool {
        self.embedded < self.inputs
    }

    /// Embedded share in `0.0..=1.0` (1.0 when there is nothing to embed).
    pub fn ratio(&self) -> f64 {
        if self.inputs == 0 {
            1.0
        } else {
            self.embedded as f64 / self.inputs as f64
        }
    }
}

/// One progress notification. Subscribe with
/// [`crate::Indexer::subscribe`]; slow subscribers may miss events (they
/// get `RecvError::Lagged`) and should re-read [`ViewStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProgressEvent {
    /// The view.
    pub view: ViewId,
    /// Its project.
    pub project: Name,
    /// The generation concerned, if any.
    pub generation: Option<i64>,
    /// The commit concerned, if any.
    pub commit: Option<String>,
    /// What happened.
    pub kind: ProgressKind,
}

/// What a [`ProgressEvent`] reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProgressKind {
    /// A build of a new target was queued.
    Queued,
    /// The active generation already matches the target.
    UpToDate,
    /// A tier changed state.
    Tier {
        /// The tier.
        tier: Tier,
        /// Its new state.
        state: TierState,
    },
    /// The generation became the one queries use.
    Activated {
        /// The generation it replaced.
        previous: Option<i64>,
    },
    /// A newer build replaced this one (the generation fence held).
    Superseded {
        /// Why.
        reason: String,
    },
    /// The view could not be synced or the build failed.
    Failed {
        /// Why.
        reason: String,
    },
    /// A personal overlay was rebuilt.
    OverlayUpdated {
        /// Paths the overlay shadows in the base view.
        shadowed: usize,
    },
}

/// In-process state of the builds this indexer ran or observed.
#[derive(Debug, Default)]
pub(crate) struct Runtime {
    pub(crate) views: BTreeMap<ViewId, ViewRuntime>,
}

/// Tier states kept per view (the newest generations this process touched:
/// a build in progress and the active generation whose T2 still runs).
const KEPT_GENERATIONS: usize = 4;

/// What this process knows about one view beyond the store.
#[derive(Debug, Clone, Default)]
pub(crate) struct ViewRuntime {
    /// Tier states by generation.
    pub(crate) builds: BTreeMap<i64, TierStates>,
    pub(crate) last_error: Option<String>,
    /// When the view was first seen behind its target.
    pub(crate) behind_since: Option<OffsetDateTime>,
    /// Whether this process synced or built the view since it started (then
    /// `last_error` is authoritative; otherwise the store's record is).
    pub(crate) observed: bool,
}

impl ViewRuntime {
    /// Tier states recorded for `generation`.
    pub(crate) fn tiers(&self, generation: i64) -> Option<&TierStates> {
        self.builds.get(&generation)
    }

    /// Records a tier change of `generation`, keeping only the newest
    /// [`KEPT_GENERATIONS`] generations.
    pub(crate) fn set_tier(&mut self, generation: i64, tier: Tier, state: TierState) {
        self.builds
            .entry(generation)
            .or_insert_with(TierStates::pending)
            .set(tier, state);
        while self.builds.len() > KEPT_GENERATIONS {
            self.builds.pop_first();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_states_serialize_with_reasons() {
        let skipped = TierState::Skipped {
            reason: TierSkip::DataPolicyLocalOnly,
        };
        assert_eq!(
            serde_json::to_string(&skipped).unwrap(),
            r#"{"state":"skipped","reason":"data_policy_local_only"}"#
        );
        let failed = TierState::Failed { reason: "x".into() };
        assert_eq!(
            serde_json::to_string(&failed).unwrap(),
            r#"{"state":"failed","reason":"x"}"#
        );
        assert_eq!(TierSkip::NoProvider.as_str(), "no_provider");
        assert!(skipped.is_final());
        assert!(!TierState::Running.is_final());
    }

    #[test]
    fn tiers_set_and_get() {
        let mut tiers = TierStates::pending();
        tiers.set(Tier::T2, TierState::Done);
        assert_eq!(tiers.get(Tier::T2), &TierState::Done);
        assert_eq!(tiers.get(Tier::T0), &TierState::Pending);
        assert_eq!(Tier::ALL.map(Tier::index), [0, 1, 2, 3]);
        assert_eq!(Tier::T3.as_str(), "t3");
    }

    #[test]
    fn runtime_keeps_tiers_per_generation() {
        let mut rt = ViewRuntime::default();
        rt.set_tier(3, Tier::T3, TierState::Done);
        rt.set_tier(4, Tier::T0, TierState::Running);
        // A late T2 of generation 3 does not disturb the build of 4.
        rt.set_tier(3, Tier::T2, TierState::Running);
        assert_eq!(rt.tiers(4).map(|t| &t.t0), Some(&TierState::Running));
        assert_eq!(rt.tiers(3).map(|t| &t.t2), Some(&TierState::Running));
        for g in 5..10 {
            rt.set_tier(g, Tier::T0, TierState::Done);
        }
        assert_eq!(rt.builds.len(), KEPT_GENERATIONS);
        assert!(rt.tiers(3).is_none());
    }

    #[test]
    fn coverage_reports_partial_and_ratio() {
        let coverage = EmbeddingCoverage {
            view: ViewId(uuid::Uuid::nil()),
            generation: 2,
            profile: ProfileId(uuid::Uuid::nil()),
            inputs: 4,
            embedded: 1,
            complete: false,
        };
        assert!(coverage.is_partial());
        assert!((coverage.ratio() - 0.25).abs() < f64::EPSILON);
        let empty = EmbeddingCoverage {
            inputs: 0,
            embedded: 0,
            ..coverage
        };
        assert!(!empty.is_partial());
        assert!((empty.ratio() - 1.0).abs() < f64::EPSILON);
        let json = serde_json::to_value(coverage).unwrap();
        assert_eq!(json["embedded"], 1);
    }

    #[test]
    fn progress_event_shape() {
        let event = ProgressEvent {
            view: ViewId(uuid::Uuid::nil()),
            project: Name::new("billing").unwrap(),
            generation: Some(2),
            commit: None,
            kind: ProgressKind::Tier {
                tier: Tier::T1,
                state: TierState::Running,
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"]["event"], "tier");
        assert_eq!(json["kind"]["tier"], "t1");
        assert_eq!(json["kind"]["state"]["state"], "running");
    }
}
