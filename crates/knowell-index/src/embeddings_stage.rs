//! T2: embeddings, after activation.
//!
//! A generation is searchable (lexically, by symbol and through the graph)
//! once T3 activated it; T2 then embeds its missing prepared inputs as
//! enrichment. Vectors are keyed by profile and prepared input, never by
//! generation, so writing them after activation is safe and whatever was
//! written stays reusable. The vector index generation of `g` is activated
//! when T2 finished, so queries that require complete vectors keep saying
//! so until then, and [`crate::Indexer::embedding_coverage`] reports how far
//! it got.
//!
//! T2 of `g` runs only while `g` is the view's active generation: it checks
//! at the start and before every provider batch, and a generation that a
//! newer one replaced stops early (its index generation is failed as
//! superseded; the newer generation's T2 embeds what is still missing). T2
//! does not hold the view's stage lock, so the next build's T0..T3 never
//! wait for slow provider calls; T2 jobs of one view run one at a time.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use knowell_core::{ContentHash, RepoPath};
use knowell_embed::{DocumentInput, EmbedError, Embedder, estimate_tokens, prepare_document};
use knowell_store::content;
use knowell_store::embeddings::{self, EmbeddingProfile, NewEmbedding};
use knowell_store::views::{self, GenerationPin};
use knowell_store::{GenerationState, PgConnection, ViewId};

use crate::analyze::parser_version_tag;
use crate::context::{EmbeddingDecision, ViewContext};
use crate::error::IndexError;
use crate::indexer::{Counters, Inner, JobRun};
use crate::jobs::StagePayload;
use crate::pipeline::{JobOutcome, Purpose, Upsert, derive_delta, files_map};
use crate::status::{Tier, TierSkip, TierState};

/// Error text of an index generation whose budget ran out (reported as
/// skipped, not failed).
pub(crate) const BUDGET_EXHAUSTED: &str = "embedding budget exhausted";
/// Files whose per-path inputs are read per store call.
const PATH_BATCH: usize = 1_000;

fn is_permanent(error: &EmbedError) -> bool {
    match error {
        EmbedError::Config(_)
        | EmbedError::InvalidInput(_)
        | EmbedError::Auth { .. }
        | EmbedError::Vector(_) => true,
        EmbedError::Http { status, .. } => (400..500).contains(status) && *status != 429,
        _ => false,
    }
}

/// How an embedding run ended.
enum EmbedOutcome {
    /// The tier reached this state.
    Finished(TierState),
    /// A newer generation became active; the run stopped early.
    Superseded(String),
}

impl<E: Embedder + 'static> Inner<E> {
    /// Why T2 of `generation` must not run, if it must not: only the active
    /// generation is embedded.
    async fn embeddings_superseded(
        &self,
        conn: &mut PgConnection,
        view: ViewId,
        generation: i64,
    ) -> Result<Option<String>, IndexError> {
        let row = views::get_view(conn, view)
            .await?
            .ok_or(IndexError::UnknownView(view))?;
        Ok(match row.active_generation {
            Some(active) if active == generation => None,
            Some(active) => Some(format!(
                "generation {generation} is no longer active (generation {active} is)"
            )),
            None => Some(format!("generation {generation} is not active")),
        })
    }

    /// The view generation that was active before `generation`, if any.
    async fn previous_active(
        &self,
        conn: &mut PgConnection,
        view: ViewId,
        generation: i64,
    ) -> Result<Option<i64>, IndexError> {
        Ok(views::list_generations(conn, view)
            .await?
            .into_iter()
            .filter(|g| g.generation < generation && g.activated_at.is_some())
            .map(|g| g.generation)
            .max())
    }

    /// T2 of an active generation.
    pub(crate) async fn stage_embeddings(
        self: &Arc<Self>,
        p: StagePayload,
        run: &JobRun,
    ) -> Result<JobOutcome, IndexError> {
        let ctx = self.context(p.view)?;
        let generation = p
            .generation
            .ok_or_else(|| IndexError::invalid("job payload", "the stage needs a generation"))?;
        let _guard = self.embed_lock(p.view).await;
        let mut conn = self.store.acquire().await?;
        if let Some(reason) = self
            .embeddings_superseded(&mut conn, p.view, generation)
            .await?
        {
            self.cache().forget((p.view, generation));
            return Ok(JobOutcome::Superseded(reason));
        }
        self.set_tier(&ctx, generation, &p.target, Tier::T2, TierState::Running);
        let outcome = match &ctx.embedding {
            EmbeddingDecision::Skip(reason) => {
                EmbedOutcome::Finished(TierState::Skipped { reason: *reason })
            }
            EmbeddingDecision::Unavailable(reason) => EmbedOutcome::Finished(TierState::Failed {
                reason: reason.clone(),
            }),
            EmbeddingDecision::Embed { provider, profile } => match self.embedders.get(provider) {
                Some(embedder) => {
                    let embedder = Arc::clone(embedder);
                    self.embed_build(&mut conn, &ctx, generation, &embedder, profile, run)
                        .await?
                }
                None => EmbedOutcome::Finished(TierState::Failed {
                    reason: format!("no embedder was given for provider `{provider}`"),
                }),
            },
        };
        self.cache().forget((p.view, generation));
        match outcome {
            EmbedOutcome::Finished(state) => {
                self.set_tier(&ctx, generation, &p.target, Tier::T2, state);
                Ok(JobOutcome::Completed)
            }
            EmbedOutcome::Superseded(reason) => {
                self.set_tier(
                    &ctx,
                    generation,
                    &p.target,
                    Tier::T2,
                    TierState::Failed {
                        reason: reason.clone(),
                    },
                );
                Ok(JobOutcome::Superseded(reason))
            }
        }
    }

    /// The prepared inputs of the given files of a generation (prepared hash
    /// → title, text). Inputs that already have a vector are listed with
    /// empty text (so counts cover them); files whose inputs are not
    /// recorded, or that miss a vector, are analysed again from stored text
    /// and their per-path inputs (re)written.
    async fn embedding_inputs(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        generation: i64,
        files: &[(RepoPath, ContentHash)],
        profile: &EmbeddingProfile,
        run: &JobRun,
    ) -> Result<BTreeMap<ContentHash, (String, String)>, IndexError> {
        let cached = {
            let cache = self.cache();
            cache
                .get((ctx.view, generation))
                .map(|a| a.analysed.clone())
                .unwrap_or_default()
        };
        let mut inputs = BTreeMap::new();
        let mut uncached = Vec::new();
        for (path, hash) in files {
            match cached.get(path) {
                Some(file) if file.content_hash == *hash => {
                    if file.embed {
                        for chunk in &file.chunks {
                            inputs.entry(chunk.input.hash).or_insert_with(|| {
                                (chunk.input.title.clone(), chunk.input.text.clone())
                            });
                        }
                    }
                }
                _ => uncached.push((path.clone(), *hash)),
            }
        }
        if uncached.is_empty() {
            return Ok(inputs);
        }
        let pin = GenerationPin {
            view: ctx.view,
            generation,
        };
        let parser = parser_version_tag();
        let paths: Vec<RepoPath> = uncached.iter().map(|(p, _)| p.clone()).collect();
        let mut recorded: BTreeMap<RepoPath, BTreeSet<ContentHash>> = BTreeMap::new();
        let mut with_rows: BTreeSet<RepoPath> = BTreeSet::new();
        for batch in paths.chunks(PATH_BATCH) {
            for input in content::chunk_inputs_at(conn, pin, &parser, Some(batch)).await? {
                with_rows.insert(input.path.clone());
                if input.embed {
                    recorded
                        .entry(input.path)
                        .or_default()
                        .insert(input.prepared_input_hash);
                }
            }
        }
        let all: Vec<ContentHash> = recorded.values().flatten().copied().collect();
        let missing: BTreeSet<ContentHash> = embeddings::missing_embeddings(conn, profile.id, &all)
            .await?
            .into_iter()
            .collect();
        for hash in &all {
            if !missing.contains(hash) {
                inputs
                    .entry(*hash)
                    .or_insert_with(|| (String::new(), String::new()));
            }
        }
        let reparse: Vec<Upsert> = uncached
            .iter()
            .filter(|(path, _)| {
                !with_rows.contains(path)
                    || recorded
                        .get(path)
                        .is_some_and(|hashes| hashes.iter().any(|h| missing.contains(h)))
            })
            .map(|(path, hash)| Upsert {
                path: path.clone(),
                hash: *hash,
                renamed_from: None,
            })
            .collect();
        if reparse.is_empty() {
            return Ok(inputs);
        }
        let analysed = self
            .analysed_upserts(conn, ctx, generation, &reparse, run, Purpose::Embeddings)
            .await?;
        // Inputs recorded before this analysis (another parser or chunking
        // configuration, or data indexed before per-path inputs existed)
        // are replaced by what the file yields now.
        self.write_chunk_inputs(conn, pin, analysed.values())
            .await?;
        for file in analysed.values() {
            if !file.embed {
                continue;
            }
            for chunk in &file.chunks {
                inputs.insert(
                    chunk.input.hash,
                    (chunk.input.title.clone(), chunk.input.text.clone()),
                );
            }
        }
        Ok(inputs)
    }

    async fn embed_build(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        generation: i64,
        embedder: &E,
        profile: &EmbeddingProfile,
        run: &JobRun,
    ) -> Result<EmbedOutcome, IndexError> {
        let pin = GenerationPin {
            view: ctx.view,
            generation,
        };
        let index_generation = embeddings::begin_index_generation(conn, pin, profile.id).await?;
        match index_generation.state {
            GenerationState::Building => {}
            GenerationState::Failed => {
                let error = index_generation
                    .error
                    .unwrap_or_else(|| "embedding failed earlier".to_owned());
                return Ok(EmbedOutcome::Finished(if error == BUDGET_EXHAUSTED {
                    TierState::Skipped {
                        reason: TierSkip::BudgetExhausted,
                    }
                } else {
                    TierState::Failed { reason: error }
                }));
            }
            GenerationState::Active | GenerationState::Retired => {
                return Ok(EmbedOutcome::Finished(TierState::Done));
            }
        }
        // Incremental when the vectors of the previously active generation
        // are complete; otherwise cover every file of the generation (inputs
        // that have a vector are reused, not sent again).
        let previous = self.previous_active(conn, ctx.view, generation).await?;
        let incremental = match previous {
            None => true,
            Some(a) => embeddings::active_index_generation(conn, ctx.view, profile.id)
                .await?
                .is_some_and(|ig| ig.view_generation == a),
        };
        let files: Vec<(RepoPath, ContentHash)> = if incremental {
            derive_delta(conn, ctx.view, generation, previous)
                .await?
                .upserts
                .into_iter()
                .map(|u| (u.path, u.hash))
                .collect()
        } else {
            files_map(conn, pin).await?.into_iter().collect()
        };
        let inputs = self
            .embedding_inputs(conn, ctx, generation, &files, profile, run)
            .await?;
        let hashes: Vec<ContentHash> = inputs.keys().copied().collect();
        let missing = embeddings::missing_embeddings(conn, profile.id, &hashes).await?;
        Counters::add(
            &self.stats.inputs_reused,
            hashes.len().saturating_sub(missing.len()) as u64,
        );
        let mut pending: Vec<(ContentHash, DocumentInput)> = missing
            .iter()
            .filter_map(|h| {
                inputs
                    .get(h)
                    .filter(|(_, text)| !text.is_empty())
                    .map(|(title, text)| {
                        (*h, DocumentInput::with_title(title.clone(), text.clone()))
                    })
            })
            .collect();
        drop(inputs);
        let kind = embedder.profile().provider_kind;
        let batch_size = self.config.embedding.batch_size.max(1);
        let mut start = 0usize;
        while start < pending.len() {
            run.check()?;
            if let Some(reason) = self
                .embeddings_superseded(conn, ctx.view, generation)
                .await?
            {
                // Vectors written so far are kept: they are keyed by input,
                // and the newer generation reuses them.
                if let Err(error) =
                    embeddings::fail_index_generation(conn, index_generation.id, &reason).await
                {
                    tracing::debug!(%error, "superseded index generation was not building");
                }
                return Ok(EmbedOutcome::Superseded(reason));
            }
            let end = start.saturating_add(batch_size).min(pending.len());
            let Some(batch) = pending.get(start..end) else {
                break;
            };
            let docs: Vec<DocumentInput> = batch.iter().map(|(_, d)| d.clone()).collect();
            let estimate: u64 = docs
                .iter()
                .map(|d| estimate_tokens(&prepare_document(kind, d)))
                .sum();
            let mut reservation = match &self.config.embedding.budget {
                Some(budget) => match budget.reserve(estimate) {
                    Ok(r) => Some(r),
                    Err(_) => {
                        embeddings::fail_index_generation(
                            conn,
                            index_generation.id,
                            BUDGET_EXHAUSTED,
                        )
                        .await?;
                        return Ok(EmbedOutcome::Finished(TierState::Skipped {
                            reason: TierSkip::BudgetExhausted,
                        }));
                    }
                },
                None => None,
            };
            Counters::add(&self.stats.embedding_calls, 1);
            match embedder.embed_documents_with_usage(&docs).await {
                Ok(embedded) => {
                    if let Some(r) = reservation.as_mut() {
                        r.add_actual(embedded.usage.input_tokens);
                    }
                    if embedded.embeddings.len() != batch.len() {
                        return Err(IndexError::Inconsistent(format!(
                            "the embedder returned {} vectors for {} inputs",
                            embedded.embeddings.len(),
                            batch.len()
                        )));
                    }
                    let rows: Vec<NewEmbedding> = batch
                        .iter()
                        .zip(embedded.embeddings)
                        .map(|((hash, _), vector)| NewEmbedding {
                            prepared_input_hash: *hash,
                            vector: vector.into_vec(),
                        })
                        .collect();
                    embeddings::upsert_embeddings(conn, profile, &rows).await?;
                    Counters::add(&self.stats.inputs_embedded, rows.len() as u64);
                    start = end;
                }
                Err(EmbedError::InputTooLong { index, .. }) => {
                    // Never truncated: the input is dropped and reported.
                    let at = start.saturating_add(index);
                    if at >= end || at >= pending.len() {
                        return Err(IndexError::Inconsistent(
                            "the embedder rejected an input outside the batch".to_owned(),
                        ));
                    }
                    let (hash, _) = pending.remove(at);
                    tracing::warn!(project = %ctx.project_name, input = %hash.short(), "embedding input is too long; not embedded");
                    Counters::add(&self.stats.inputs_rejected, 1);
                }
                Err(EmbedError::BudgetExceeded(_)) => {
                    embeddings::fail_index_generation(conn, index_generation.id, BUDGET_EXHAUSTED)
                        .await?;
                    return Ok(EmbedOutcome::Finished(TierState::Skipped {
                        reason: TierSkip::BudgetExhausted,
                    }));
                }
                Err(error) if is_permanent(&error) || run.is_last_attempt() => {
                    let reason = error.to_string();
                    embeddings::fail_index_generation(conn, index_generation.id, &reason).await?;
                    return Ok(EmbedOutcome::Finished(TierState::Failed { reason }));
                }
                // Retried by the queue; vectors stored so far are kept.
                Err(error) => return Err(error.into()),
            }
        }
        let coverage =
            embeddings::input_coverage(conn, pin, profile.id, &parser_version_tag()).await?;
        embeddings::update_index_counts(
            conn,
            index_generation.id,
            coverage.inputs,
            coverage.embedded,
        )
        .await?;
        // A run that finished every batch activates its vectors even if the
        // view moved on meanwhile: the newer generation's T2 then only needs
        // the difference. The fence refuses an older activation.
        match embeddings::activate_index_generation(conn, index_generation.id).await {
            Ok(_) => Ok(EmbedOutcome::Finished(TierState::Done)),
            Err(
                error @ (knowell_store::StoreError::StaleGeneration { .. }
                | knowell_store::StoreError::GenerationNotBuilding { .. }),
            ) => Ok(EmbedOutcome::Superseded(error.to_string())),
            Err(error) => Err(error.into()),
        }
    }
}
