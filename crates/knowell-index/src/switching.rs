//! Blue-green embedding profile switches in the indexer.
//!
//! The store records which profile each view serves and which switches are
//! building ([`knowell_store::switches`]); that record, not this process, is
//! the truth, so switches survive restarts. Here:
//!
//! - which profiles T2 builds for a view: the one it serves, then the target
//!   of the building switch it belongs to, so queries never lose semantic
//!   search while a switch builds;
//! - which configured embedder produces a profile, matched by identity
//!   (provider kind, model, dimensions, input format), never a substitute;
//! - catching a switch's target up on the views' active generations;
//! - starting switches when the configured profile changes (only where
//!   indexing is scheduled, never on registration) and resuming building
//!   ones;
//! - activating a switch once its target covers every member view.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use knowell_config::DataPolicy;
use knowell_embed::{Embedder, ProviderKind as EmbedProviderKind};
use knowell_store::embeddings::{self, EmbeddingProfile};
use knowell_store::switches::{
    self, ConfiguredProfile, NewSwitch, ProfileSwitch, SwitchActivation, SwitchOrigin,
};
use knowell_store::views::{self, GenerationPin};
use knowell_store::{
    GenerationState, JobId, JobState, OrganizationId, PgConnection, ProfileId, ProfileSwitchId,
    StoreError, ViewId, WorkspaceId, jobs,
};
use serde::Serialize;

use crate::config::Priority;
use crate::context::{EmbeddingDecision, ViewContext, store_profile};
use crate::error::IndexError;
use crate::indexer::{Indexer, Inner};
use crate::jobs::{BuildTarget, StagePayload, stage_job, stage_key};
use crate::pipeline::store_tree_hash;
use crate::status::{Tier, TierSkip};

/// Rollback window of switches started for a configuration change: 7 days,
/// in seconds.
pub const CONFIGURATION_SWITCH_RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Who produces the vectors of a profile for a view.
pub(crate) enum Producer<E> {
    /// This configured embedder.
    Embed {
        /// The embedder.
        embedder: Arc<E>,
        /// The profile it produces.
        profile: EmbeddingProfile,
    },
    /// Deliberately not produced (the data policy forbids the provider).
    Skip(TierSkip),
    /// No configured embedder produces it (no secret values in the text).
    Unavailable(String),
}

/// How far a switch's target covers one member view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwitchViewProgress {
    /// The view.
    pub view: ViewId,
    /// Its active generation (`None`: nothing built yet, counts as covered).
    pub active_generation: Option<i64>,
    /// Whether the target's vectors cover that generation completely.
    pub covered: bool,
    /// Inputs of the active generation meant to be embedded.
    pub inputs: u64,
    /// Of those, the inputs with a vector in the target profile.
    pub embedded: u64,
    /// Why building the target for that generation failed, if it did.
    pub failure: Option<String>,
}

impl<E: Embedder + 'static> Inner<E> {
    /// The profiles T2 builds for `ctx`, the serving one first. Empty when
    /// the configuration does not embed (then T2 reports the configuration's
    /// skip or failure instead).
    pub(crate) async fn embedding_targets(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
    ) -> Result<Vec<ProfileId>, IndexError> {
        let EmbeddingDecision::Embed { profile, .. } = &ctx.embedding else {
            return Ok(Vec::new());
        };
        // A view that serves nothing yet serves its configured profile once
        // the change is acknowledged; building it now is the intended work.
        let serving = switches::view_embeddings(conn, &[ctx.view])
            .await?
            .get(&ctx.view)
            .and_then(|row| row.serving)
            .unwrap_or(profile.id);
        let mut targets = vec![serving];
        if let Some(switch) = switches::building_switch_of_view(conn, ctx.view).await?
            && !targets.contains(&switch.to)
        {
            targets.push(switch.to);
        }
        Ok(targets)
    }

    /// The configured embedder that produces `profile`'s vectors for `ctx`,
    /// matched by identity; never another profile's embedder.
    pub(crate) async fn producer(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        profile: ProfileId,
    ) -> Result<Producer<E>, IndexError> {
        if let EmbeddingDecision::Embed {
            provider,
            profile: configured,
        } = &ctx.embedding
            && configured.id == profile
        {
            return Ok(match self.embedders.get(provider) {
                Some(embedder) => Producer::Embed {
                    embedder: Arc::clone(embedder),
                    profile: configured.clone(),
                },
                None => Producer::Unavailable(format!(
                    "no embedder was given for provider `{provider}`"
                )),
            });
        }
        let Some(stored) =
            embeddings::get_profile_in_organization(conn, ctx.organization, profile).await?
        else {
            return Ok(Producer::Unavailable(format!(
                "embedding profile {profile} does not exist in this organization"
            )));
        };
        for (name, embedder) in &self.embedders {
            let spec = store_profile(embedder.profile())?;
            let same = spec.provider == stored.provider
                && spec.model == stored.model
                && spec.dimensions == stored.dimensions
                && spec.input_format_version == stored.input_format_version;
            if !same {
                continue;
            }
            let cloud = self.providers.get(name).is_some_and(|c| c.kind.is_cloud())
                || embedder.profile().provider_kind == EmbedProviderKind::Gemini;
            if ctx.data_policy == DataPolicy::LocalOnly && cloud {
                return Ok(Producer::Skip(TierSkip::DataPolicyLocalOnly));
            }
            return Ok(Producer::Embed {
                embedder: Arc::clone(embedder),
                profile: stored,
            });
        }
        Ok(Producer::Unavailable(format!(
            "no configured embedder produces embedding profile `{}`",
            stored.name
        )))
    }

    /// Activates the building switch `view` belongs to if it moves to
    /// `profile` and now covers every member view.
    pub(crate) async fn advance_switch(
        &self,
        conn: &mut PgConnection,
        view: ViewId,
        profile: ProfileId,
    ) -> Result<(), IndexError> {
        if let Some(switch) = switches::building_switch_of_view(conn, view).await?
            && switch.to == profile
        {
            self.try_activate(conn, &switch).await?;
        }
        Ok(())
    }

    async fn try_activate(
        &self,
        conn: &mut PgConnection,
        switch: &ProfileSwitch,
    ) -> Result<(), IndexError> {
        if let SwitchActivation::Activated(active) =
            switches::activate_switch(conn, switch.organization, switch.id).await?
        {
            tracing::info!(
                switch = %active.id,
                to = %active.to,
                views = active.views.len(),
                "embedding profile switch activated"
            );
        }
        Ok(())
    }

    /// The build target of the active `generation` of `view`: its commit, or
    /// the directory tree it was built from.
    async fn active_target(
        &self,
        conn: &mut PgConnection,
        active_commit: Option<&str>,
        pin: GenerationPin,
    ) -> Result<BuildTarget, IndexError> {
        Ok(match active_commit {
            Some(id) => BuildTarget::Commit { id: id.to_owned() },
            None => BuildTarget::Tree {
                hash: store_tree_hash(conn, pin).await?,
            },
        })
    }

    /// Queues T2 of the switch's target for every member view this indexer
    /// registered whose active generation the target does not cover and no
    /// live job will build, then activates the switch if it covers every
    /// view. With `retry_failed` (an explicit request, never the periodic
    /// reconciliation, so a permanent provider error is not retried in a
    /// loop) a failed build of the target is started again. Returns the jobs
    /// it queued.
    pub(crate) async fn catch_up(
        &self,
        switch: &ProfileSwitch,
        retry_failed: bool,
    ) -> Result<Vec<JobId>, IndexError> {
        let mut conn = self.store.acquire().await?;
        let mut queued = Vec::new();
        for view in &switch.views {
            // Views registered by another process are caught up there.
            if self.context(*view).is_err() {
                continue;
            }
            let Some(row) = views::get_view(&mut conn, *view).await? else {
                continue;
            };
            let Some(generation) = row.active_generation else {
                continue;
            };
            let pin = GenerationPin {
                view: *view,
                generation,
            };
            let started = embeddings::index_generation_at(&mut conn, pin, switch.to).await?;
            let mut retry = false;
            match started.as_ref().map(|ig| ig.state) {
                None | Some(GenerationState::Building) => {}
                Some(GenerationState::Failed) if retry_failed => {
                    if let Some(ig) = &started {
                        retry = embeddings::restart_index_generation(&mut conn, ig.id).await?;
                    }
                    if !retry {
                        continue;
                    }
                }
                Some(_) => continue,
            }
            let target = self
                .active_target(&mut conn, row.active_commit.as_deref(), pin)
                .await?;
            // The generation's own T2, while it is live, builds every target.
            let own = stage_key(Tier::T2, *view, &target, generation);
            let live = jobs::find_job_by_key(&mut conn, &own)
                .await?
                .is_some_and(|job| {
                    matches!(
                        job.state,
                        JobState::Queued | JobState::Running | JobState::Failed
                    )
                });
            if live {
                continue;
            }
            let payload = StagePayload {
                view: *view,
                target,
                generation: Some(generation),
                priority: Priority::Background,
                force: false,
                profile: Some(switch.to),
            };
            let mut job = stage_job(Tier::T2, &payload, generation, &self.config.jobs)?;
            if retry {
                // The earlier catch-up of this generation already ran.
                job.idempotency_key = job
                    .idempotency_key
                    .map(|key| format!("{key}:retry-{}", uuid::Uuid::now_v7()));
            }
            let enqueued =
                jobs::enqueue_scoped(&mut conn, &job, jobs::JobScope::View(*view)).await?;
            self.wake_workers();
            if enqueued.created {
                queued.push(enqueued.id);
            }
        }
        self.try_activate(&mut conn, switch).await?;
        Ok(queued)
    }

    /// Starts a switch for every group of views whose configured profile
    /// changed since it was acknowledged, then resumes every building switch
    /// of the registered organizations (see [`Inner::catch_up`] for
    /// `retry_failed`). Runs where indexing is scheduled
    /// (`Indexer::index_workspace`, reconciliation), never on registration.
    pub(crate) async fn reconcile_profiles(&self, retry_failed: bool) -> Result<(), IndexError> {
        let contexts = self.contexts();
        let mut conn = self.store.acquire().await?;
        type Group = (OrganizationId, WorkspaceId, ProfileId, ProfileId);
        let mut groups: BTreeMap<Group, Vec<ViewId>> = BTreeMap::new();
        for ctx in &contexts {
            let configured = match &ctx.embedding {
                EmbeddingDecision::Embed { profile, .. } => Some(profile.id),
                _ => None,
            };
            let ConfiguredProfile::Differs(row) =
                switches::record_configured_profile(&mut conn, ctx.view, configured).await?
            else {
                continue;
            };
            match (row.serving, configured) {
                (Some(serving), Some(to)) if serving != to => {
                    groups
                        .entry((ctx.organization, ctx.workspace, serving, to))
                        .or_default()
                        .push(ctx.view);
                }
                // Nothing served yet, the configuration names what serves
                // already, or it stopped embedding (what serves keeps serving
                // the generations its vectors cover).
                _ => {
                    switches::acknowledge_configured_profile(&mut conn, ctx.view, configured)
                        .await?;
                }
            }
        }
        for ((organization, workspace, from, to), views) in groups {
            let new = NewSwitch {
                organization,
                workspace,
                from: Some(from),
                to,
                views: views.clone(),
                origin: SwitchOrigin::Configuration,
                requested_by: "configuration".to_owned(),
                retention_seconds: CONFIGURATION_SWITCH_RETENTION_SECONDS,
            };
            match switches::start_switch(&mut conn, &new).await {
                Ok(switch) => {
                    for view in &views {
                        switches::acknowledge_configured_profile(&mut conn, *view, Some(to))
                            .await?;
                    }
                    tracing::info!(switch = %switch.id, %from, %to, views = views.len(), "the configured embedding profile changed; switching");
                }
                // Not acknowledged: the next reconciliation starts it once
                // the building switch has ended.
                Err(StoreError::AlreadyExists { .. }) => {
                    tracing::warn!(%workspace, %to, "the configured embedding profile changed while another profile switch builds; it starts after that one ends");
                }
                Err(error) => return Err(error.into()),
            }
        }
        let organizations: BTreeSet<OrganizationId> =
            contexts.iter().map(|c| c.organization).collect();
        drop(conn);
        for organization in organizations {
            let mut conn = self.store.acquire().await?;
            let building = switches::building_switches(&mut conn, organization).await?;
            drop(conn);
            for switch in building {
                self.catch_up(&switch, retry_failed).await?;
            }
        }
        Ok(())
    }

    /// The organization of the registered views.
    fn organization(&self) -> Result<OrganizationId, IndexError> {
        self.contexts()
            .first()
            .map(|c| c.organization)
            .ok_or_else(|| IndexError::invalid("profile switch", "no workspace is registered"))
    }
}

impl<E: Embedder + 'static> Indexer<E> {
    /// Starts switching `views` (registered, of one workspace, serving one
    /// profile) to `to`. While it builds, T2 builds both profiles and the
    /// old one keeps serving; it activates by itself once `to` covers every
    /// view's active generation. A rollback is accepted for
    /// `retention_seconds` after activation. Returns the switch and the
    /// catch-up jobs it queued.
    ///
    /// # Errors
    /// [`IndexError::Invalid`] when the views are not registered, span
    /// workspaces, serve different profiles or `to` already, or no configured
    /// embedder may produce `to` for one of them; store errors (another
    /// switch building in the workspace is [`StoreError::AlreadyExists`]).
    pub async fn start_switch(
        &self,
        views: &[ViewId],
        to: ProfileId,
        requested_by: &str,
        retention_seconds: u64,
    ) -> Result<(ProfileSwitch, Vec<JobId>), IndexError> {
        let inner = &self.inner;
        let contexts = views
            .iter()
            .map(|v| inner.context(*v))
            .collect::<Result<Vec<_>, _>>()?;
        let Some(first) = contexts.first() else {
            return Err(IndexError::invalid(
                "profile switch",
                "a switch needs at least one view",
            ));
        };
        if contexts.iter().any(|c| c.workspace != first.workspace) {
            return Err(IndexError::invalid(
                "profile switch",
                "the views belong to different workspaces; switch them separately",
            ));
        }
        let mut conn = inner.store.acquire().await?;
        let rows = switches::view_embeddings(&mut conn, views).await?;
        let serving: BTreeSet<Option<ProfileId>> = views
            .iter()
            .map(|v| rows.get(v).and_then(|row| row.serving))
            .collect();
        let from = match serving.into_iter().collect::<Vec<_>>().as_slice() {
            [single] => *single,
            _ => {
                return Err(IndexError::invalid(
                    "profile switch",
                    "the views serve different profiles; switch them separately",
                ));
            }
        };
        if from == Some(to) {
            return Err(IndexError::invalid(
                "profile switch",
                "the views already serve this profile",
            ));
        }
        for ctx in &contexts {
            match inner.producer(&mut conn, ctx, to).await? {
                Producer::Embed { .. } => {}
                Producer::Skip(_) => {
                    return Err(IndexError::invalid(
                        "profile switch",
                        format!(
                            "{} is local-only and this profile's provider is a cloud service",
                            ctx.project_name
                        ),
                    ));
                }
                Producer::Unavailable(reason) => {
                    return Err(IndexError::invalid("profile switch", reason));
                }
            }
        }
        let switch = switches::start_switch(
            &mut conn,
            &NewSwitch {
                organization: first.organization,
                workspace: first.workspace,
                from,
                to,
                views: views.to_vec(),
                origin: SwitchOrigin::Request,
                requested_by: requested_by.to_owned(),
                retention_seconds,
            },
        )
        .await?;
        drop(conn);
        let jobs = inner.catch_up(&switch, true).await?;
        Ok((self.switch(switch.id).await?.unwrap_or(switch), jobs))
    }

    /// Cancels a building switch; the old profile keeps serving.
    ///
    /// # Errors
    /// Store errors ([`StoreError::InvalidInput`] when it is not building,
    /// [`StoreError::NotFound`] when unknown).
    pub async fn cancel_switch(&self, id: ProfileSwitchId) -> Result<ProfileSwitch, IndexError> {
        let organization = self.inner.organization()?;
        let mut conn = self.inner.store.acquire().await?;
        Ok(switches::cancel_switch(&mut conn, organization, id).await?)
    }

    /// Starts the switch reversing the active switch `id` within its
    /// retention. When the old profile still covers the active generations
    /// (no commit since), it activates at once without provider calls.
    ///
    /// # Errors
    /// Store errors ([`StoreError::InvalidInput`] when it cannot be rolled
    /// back, [`StoreError::AlreadyExists`] while another switch builds).
    pub async fn rollback_switch(
        &self,
        id: ProfileSwitchId,
        requested_by: &str,
    ) -> Result<ProfileSwitch, IndexError> {
        let organization = self.inner.organization()?;
        let mut conn = self.inner.store.acquire().await?;
        let reverse = switches::start_rollback(&mut conn, organization, id, requested_by).await?;
        drop(conn);
        self.inner.catch_up(&reverse, true).await?;
        Ok(self.switch(reverse.id).await?.unwrap_or(reverse))
    }

    /// One switch of the registered organization.
    ///
    /// # Errors
    /// Store errors; [`IndexError::Invalid`] when nothing is registered.
    pub async fn switch(&self, id: ProfileSwitchId) -> Result<Option<ProfileSwitch>, IndexError> {
        let organization = self.inner.organization()?;
        let mut conn = self.inner.store.acquire().await?;
        Ok(switches::get_switch(&mut conn, organization, id).await?)
    }

    /// The newest `limit` switches of the registered organization.
    ///
    /// # Errors
    /// Store errors; [`IndexError::Invalid`] when nothing is registered.
    pub async fn switches(&self, limit: u32) -> Result<Vec<ProfileSwitch>, IndexError> {
        let organization = self.inner.organization()?;
        let mut conn = self.inner.store.acquire().await?;
        Ok(switches::list_switches(&mut conn, organization, limit).await?)
    }

    /// How far the switch's target covers each member view's active
    /// generation, in view order.
    ///
    /// # Errors
    /// Store errors.
    pub async fn switch_progress(
        &self,
        switch: &ProfileSwitch,
    ) -> Result<Vec<SwitchViewProgress>, IndexError> {
        let mut conn = self.inner.store.acquire().await?;
        let parser = crate::analyze::parser_version_tag();
        let mut out = Vec::with_capacity(switch.views.len());
        for view in &switch.views {
            let active_generation = views::get_view(&mut conn, *view)
                .await?
                .and_then(|row| row.active_generation);
            let mut progress = SwitchViewProgress {
                view: *view,
                active_generation,
                covered: active_generation.is_none(),
                inputs: 0,
                embedded: 0,
                failure: None,
            };
            if let Some(generation) = active_generation {
                let pin = GenerationPin {
                    view: *view,
                    generation,
                };
                if let Some(ig) = embeddings::index_generation_at(&mut conn, pin, switch.to).await?
                {
                    progress.covered =
                        matches!(ig.state, GenerationState::Active | GenerationState::Retired);
                    if ig.state == GenerationState::Failed {
                        progress.failure = ig.error;
                    }
                }
                match embeddings::input_coverage(&mut conn, pin, switch.to, &parser).await {
                    Ok(coverage) => {
                        progress.inputs = coverage.inputs;
                        progress.embedded = coverage.embedded;
                    }
                    Err(StoreError::SemanticUnavailable) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            out.push(progress);
        }
        Ok(out)
    }
}
