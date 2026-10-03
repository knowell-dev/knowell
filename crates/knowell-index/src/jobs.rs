//! Job kinds and payloads of the indexing pipeline.
//!
//! A build of one view target is a chain of durable jobs, one per stage, in
//! the order T0 → T1 → T3 (activation) → T2. Each stage enqueues the next
//! one when it succeeds, so a crash resumes at the stage that was running, a
//! provider outage retries only the embedding stage (after the generation is
//! already searchable), and every stage is scheduled by its own priority.
//! Every job is attributed to its view (`enqueue_scoped`), and an indexer
//! claims only the jobs of the views it registered.
//!
//! | Kind | Does | Idempotency key |
//! |---|---|---|
//! | `index.sync` | resolve the target, record the seen commit, queue a build | none (coalesced in process) |
//! | `index.text` (T0) | begin or resume the generation, plan changes, store content and file versions, build the lexical index | `index.text:<view>:<target>:n<last generation>` |
//! | `index.symbols` (T1) | parse changed files: chunks and per-path inputs, symbols, definitions and references, syntactic edges; re-resolve invalidated dependents | `index.symbols:<view>:<target>:g<generation>` |
//! | `index.relations` (T3) | relation stage (contract linking), staleness set, then activation behind the fence; queues T2 | `index.relations:<view>:<target>:g<generation>` |
//! | `index.embeddings` (T2) | after activation: embed missing prepared inputs under the data policy and budget into the profile the view serves and the target of a building profile switch, then activate their vector index generations | `index.embeddings:<view>:<target>:g<generation>`, or `...:p<profile>` when a switch catches up on one profile |
//!
//! `<target>` is the commit id, or `tree-<hash>` for directory sources.

use knowell_core::ContentHash;
use knowell_store::jobs::NewJob;
use knowell_store::{ProfileId, ViewId};
use serde::{Deserialize, Serialize};

use crate::config::{JobSettings, Priority};
use crate::error::IndexError;
use crate::status::Tier;

/// Resolve a view's target and queue a build when it moved.
pub const JOB_SYNC: &str = "index.sync";
/// T0: text and paths.
pub const JOB_TEXT: &str = "index.text";
/// T1: symbols, chunks, syntactic edges.
pub const JOB_SYMBOLS: &str = "index.symbols";
/// T2: embeddings.
pub const JOB_EMBEDDINGS: &str = "index.embeddings";
/// T3: relations, staleness, activation.
pub const JOB_RELATIONS: &str = "index.relations";

/// Every job kind this crate runs.
pub const JOB_KINDS: [&str; 5] = [
    JOB_SYNC,
    JOB_TEXT,
    JOB_SYMBOLS,
    JOB_EMBEDDINGS,
    JOB_RELATIONS,
];

/// The stage job that produces `tier`.
pub fn stage_kind(tier: Tier) -> &'static str {
    match tier {
        Tier::T0 => JOB_TEXT,
        Tier::T1 => JOB_SYMBOLS,
        Tier::T2 => JOB_EMBEDDINGS,
        Tier::T3 => JOB_RELATIONS,
    }
}

/// The tier a stage job kind produces.
pub fn kind_tier(kind: &str) -> Option<Tier> {
    match kind {
        JOB_TEXT => Some(Tier::T0),
        JOB_SYMBOLS => Some(Tier::T1),
        JOB_EMBEDDINGS => Some(Tier::T2),
        JOB_RELATIONS => Some(Tier::T3),
        _ => None,
    }
}

/// What a build targets: a commit, or a directory tree state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BuildTarget {
    /// A git commit (full lowercase hex id).
    Commit {
        /// The commit.
        id: String,
    },
    /// A directory source at a tree hash.
    Tree {
        /// Tree hash of the directory when the build was queued.
        hash: ContentHash,
    },
}

impl BuildTarget {
    /// The commit, for git targets.
    pub fn commit(&self) -> Option<&str> {
        match self {
            BuildTarget::Commit { id } => Some(id),
            BuildTarget::Tree { .. } => None,
        }
    }

    /// Text used in idempotency keys.
    pub fn key(&self) -> String {
        match self {
            BuildTarget::Commit { id } => id.clone(),
            BuildTarget::Tree { hash } => format!("tree-{hash}"),
        }
    }
}

/// Payload of `index.sync`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPayload {
    /// The view.
    pub view: ViewId,
    /// Scheduling class of the build it may queue.
    pub priority: Priority,
    /// Force a full rebuild even when the target did not move.
    #[serde(default)]
    pub force: bool,
}

/// Payload of the stage jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagePayload {
    /// The view.
    pub view: ViewId,
    /// What is being built.
    pub target: BuildTarget,
    /// The generation (absent for T0, which allocates it).
    #[serde(default)]
    pub generation: Option<i64>,
    /// Scheduling class of the build.
    pub priority: Priority,
    /// Build every file again instead of planning from the active
    /// generation (reconciliation found the store out of step).
    #[serde(default)]
    pub force: bool,
    /// T2 only: build just this profile (a profile switch catching up on an
    /// active generation); `None` builds every profile the view needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileId>,
}

pub(crate) fn decode<T: for<'de> Deserialize<'de>>(
    payload: &serde_json::Value,
) -> Result<T, IndexError> {
    serde_json::from_value(payload.clone())
        .map_err(|e| IndexError::invalid("job payload", e.to_string()))
}

fn encode<T: Serialize>(payload: &T) -> Result<serde_json::Value, IndexError> {
    serde_json::to_value(payload).map_err(|e| IndexError::invalid("job payload", e.to_string()))
}

/// The `index.sync` job for a view (no idempotency key: syncs are cheap and
/// coalesced by the caller).
pub(crate) fn sync_job(
    payload: &SyncPayload,
    settings: &JobSettings,
) -> Result<NewJob, IndexError> {
    let mut job = NewJob::new(JOB_SYNC, encode(payload)?);
    job.priority = payload.priority.value();
    job.max_attempts = settings.max_attempts;
    Ok(job)
}

/// The T0 job of a build. `last_generation` is the view's generation
/// counter when the build was queued: two triggers for the same target in
/// the same state share one job, while a target whose earlier build failed
/// can be queued again.
pub(crate) fn text_job(
    payload: &StagePayload,
    last_generation: i64,
    settings: &JobSettings,
) -> Result<NewJob, IndexError> {
    let mut job = NewJob::new(JOB_TEXT, encode(payload)?);
    job.priority = payload.priority.value();
    job.max_attempts = settings.max_attempts;
    let force = if payload.force { ":force" } else { "" };
    job.idempotency_key = Some(format!(
        "{JOB_TEXT}:{}:{}:n{last_generation}{force}",
        payload.view,
        payload.target.key()
    ));
    Ok(job)
}

/// The idempotency key of the stage job producing `tier` for `generation`.
pub(crate) fn stage_key(tier: Tier, view: ViewId, target: &BuildTarget, generation: i64) -> String {
    format!("{}:{view}:{}:g{generation}", stage_kind(tier), target.key())
}

/// The idempotency key of a T2 job that builds only `profile`.
pub(crate) fn profile_stage_key(
    view: ViewId,
    target: &BuildTarget,
    generation: i64,
    profile: ProfileId,
) -> String {
    format!(
        "{}:p{profile}",
        stage_key(Tier::T2, view, target, generation)
    )
}

/// The job of a later stage (T1..T3) of a build of `generation`.
pub(crate) fn stage_job(
    tier: Tier,
    payload: &StagePayload,
    generation: i64,
    settings: &JobSettings,
) -> Result<NewJob, IndexError> {
    let kind = stage_kind(tier);
    let payload = StagePayload {
        generation: Some(generation),
        ..payload.clone()
    };
    let mut job = NewJob::new(kind, encode(&payload)?);
    job.priority = if tier == Tier::T2 {
        payload.priority.embedding_value()
    } else {
        payload.priority.value()
    };
    job.max_attempts = settings.max_attempts;
    job.idempotency_key = Some(match payload.profile {
        Some(profile) if tier == Tier::T2 => {
            profile_stage_key(payload.view, &payload.target, generation, profile)
        }
        _ => stage_key(tier, payload.view, &payload.target, generation),
    });
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> StagePayload {
        StagePayload {
            view: ViewId(uuid::Uuid::nil()),
            target: BuildTarget::Commit { id: "a".repeat(40) },
            generation: None,
            priority: Priority::Active,
            force: false,
            profile: None,
        }
    }

    #[test]
    fn keys_name_view_target_and_stage() {
        let settings = JobSettings::default();
        let t0 = text_job(&payload(), 3, &settings).unwrap();
        let key = t0.idempotency_key.unwrap();
        assert!(key.starts_with("index.text:00000000-0000-0000-0000-000000000000:"));
        assert!(key.ends_with(":n3"));
        let t2 = stage_job(Tier::T2, &payload(), 4, &settings).unwrap();
        assert_eq!(t2.kind, JOB_EMBEDDINGS);
        assert_eq!(
            t2.idempotency_key.as_deref(),
            Some(stage_key(Tier::T2, payload().view, &payload().target, 4).as_str())
        );
        assert!(t2.idempotency_key.unwrap().ends_with(":g4"));
        assert_eq!(t2.priority, Priority::Active.embedding_value());
        let back: StagePayload = decode(&t2.payload).unwrap();
        assert_eq!(back.generation, Some(4));
        let forced = text_job(
            &StagePayload {
                force: true,
                ..payload()
            },
            3,
            &settings,
        )
        .unwrap();
        assert_ne!(
            forced.idempotency_key,
            text_job(&payload(), 3, &settings).unwrap().idempotency_key
        );
    }

    #[test]
    fn profile_jobs_have_their_own_keys_and_old_payloads_still_decode() {
        let settings = JobSettings::default();
        let profile = ProfileId(uuid::Uuid::from_u128(7));
        let all = stage_job(Tier::T2, &payload(), 4, &settings).unwrap();
        let one = stage_job(
            Tier::T2,
            &StagePayload {
                profile: Some(profile),
                ..payload()
            },
            4,
            &settings,
        )
        .unwrap();
        assert_ne!(all.idempotency_key, one.idempotency_key);
        assert_eq!(
            one.idempotency_key.as_deref(),
            Some(profile_stage_key(payload().view, &payload().target, 4, profile).as_str())
        );
        let back: StagePayload = decode(&one.payload).unwrap();
        assert_eq!(back.profile, Some(profile));
        // Payloads written before profiles existed decode to "every profile".
        let legacy = serde_json::json!({
            "view": payload().view,
            "target": {"type": "commit", "id": "a".repeat(40)},
            "generation": 4,
            "priority": "active",
        });
        assert_eq!(decode::<StagePayload>(&legacy).unwrap().profile, None);
        assert!(all.payload.get("profile").is_none());
    }

    #[test]
    fn kinds_and_tiers_round_trip() {
        for tier in Tier::ALL {
            assert_eq!(kind_tier(stage_kind(tier)), Some(tier));
        }
        assert_eq!(kind_tier(JOB_SYNC), None);
        assert!(decode::<StagePayload>(&serde_json::json!({"view": 1})).is_err());
    }

    #[test]
    fn tree_targets_have_keys() {
        let t = BuildTarget::Tree {
            hash: ContentHash::of(b"x"),
        };
        assert!(t.key().starts_with("tree-"));
        assert_eq!(t.commit(), None);
    }
}
