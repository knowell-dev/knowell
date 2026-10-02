//! T3: the relation stage (contract linking by default), the staleness set,
//! then activation behind the generation fence. T2 is queued once the
//! generation is active.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use knowell_core::{ContentHash, RepoPath};
use knowell_embed::Embedder;
use knowell_link::ProjectExtractions;
use knowell_store::graph;
use knowell_store::symbols;
use knowell_store::views::{self, GenerationPin};
use knowell_store::{GenerationState, PgConnection, StoreError, ViewId};

use crate::context::{EmbeddingDecision, ViewContext};
use crate::error::IndexError;
use crate::indexer::{Counters, Inner, JobRun};
use crate::jobs::StagePayload;
use crate::link::{
    CachedExtractions, LinkRelationStage, RowContext, SymbolIndex, changed_origins, origin, rows,
};
use crate::manifest;
use crate::pipeline::{Delta, JobOutcome, Purpose, derive_delta, files_map};
use crate::relate::{
    RelationError, RelationFile, RelationInput, RelationOutput, StaleFile, StalenessEvent,
    validate_output,
};
use crate::status::{ProgressKind, Tier, TierState};

/// Paths whose stored definitions are read per store call.
const PATH_BATCH: usize = 1_000;

impl<E: Embedder + 'static> Inner<E> {
    /// The changed files as a custom relation stage sees them.
    async fn relation_files(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        generation: i64,
        delta: &Delta,
        run: &JobRun,
    ) -> Result<Vec<RelationFile>, IndexError> {
        let analysed = self
            .analysed_upserts(
                conn,
                ctx,
                generation,
                &delta.upserts,
                run,
                Purpose::Relations,
            )
            .await?;
        Ok(analysed
            .into_values()
            .map(|file| RelationFile {
                path: file.path.clone(),
                content_hash: file.content_hash,
                text: Arc::clone(&file.text),
                parsed: Arc::clone(&file.parsed),
            })
            .collect())
    }

    /// Runs a custom [`crate::RelationStage`] on a blocking thread.
    async fn custom_relations(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        generation: i64,
        delta: &Delta,
        run: &JobRun,
    ) -> Result<Result<RelationOutput, RelationError>, IndexError> {
        let changed = self
            .relation_files(conn, ctx, generation, delta, run)
            .await?;
        let removed = delta.removed.clone();
        let stage = Arc::clone(&self.relations);
        let (workspace, project, project_name, view) = (
            ctx.workspace,
            ctx.project,
            ctx.project_name.clone(),
            ctx.view,
        );
        Ok(tokio::task::spawn_blocking(move || {
            let input = RelationInput {
                workspace,
                project,
                project_name: &project_name,
                view,
                generation,
                changed: &changed,
                removed: &removed,
            };
            let output = stage.relate(&input)?;
            validate_output(stage.name(), &output)?;
            Ok::<_, RelationError>(output)
        })
        .await?)
    }

    /// Extractions of the other projects of the workspace this indexer
    /// serves, at their active generations: from the link stage's cache, or
    /// extracted from stored text when the cache is cold.
    async fn workspace_extractions(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        link: &Arc<LinkRelationStage>,
    ) -> Result<Vec<Arc<ProjectExtractions>>, IndexError> {
        // One view per other project (the lowest view id), deterministic.
        let mut by_project: BTreeMap<knowell_store::ProjectId, Arc<ViewContext>> = BTreeMap::new();
        for other in self.contexts() {
            if other.workspace != ctx.workspace || other.project == ctx.project {
                continue;
            }
            let keep = by_project
                .get(&other.project)
                .is_none_or(|existing| other.view < existing.view);
            if keep {
                by_project.insert(other.project, other);
            }
        }
        let mut out = Vec::new();
        for other in by_project.into_values() {
            let Some(row) = views::get_view(conn, other.view).await? else {
                continue;
            };
            let Some(active) = row.active_generation else {
                continue;
            };
            if let Some(cached) = link.cached(other.view)
                && cached.generation == active
            {
                out.push(cached.extractions);
                continue;
            }
            let pin = GenerationPin {
                view: other.view,
                generation: active,
            };
            let files = files_map(conn, pin).await?;
            if files.len() > link.max_files() {
                tracing::warn!(project = %other.project_name, files = files.len(), "project above the link file bound is not linked");
                continue;
            }
            let mut texts = BTreeMap::new();
            self.fill_texts(conn, &other, files.values().copied(), &mut texts)
                .await?;
            let list: Vec<(RepoPath, Arc<str>)> = files
                .iter()
                .filter_map(|(path, hash)| texts.get(hash).map(|t| (path.clone(), Arc::clone(t))))
                .collect();
            let stage = Arc::clone(link);
            let other_ctx = Arc::clone(&other);
            let extracted = tokio::task::spawn_blocking(move || {
                stage.extract(&other_ctx.project_name, &list, &other_ctx.policy)
            })
            .await?;
            let extracted = Arc::new(extracted);
            link.remember(
                other.view,
                CachedExtractions {
                    generation: active,
                    workspace: other.workspace,
                    extractions: Arc::clone(&extracted),
                },
            );
            out.push(extracted);
        }
        Ok(out)
    }

    /// The store-backed link stage (see `crate::link`): extraction of the
    /// whole project at `generation`, linking with the workspace, and only
    /// the origins whose rows changed.
    async fn link_relations(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        generation: i64,
        delta: &Delta,
        link: &Arc<LinkRelationStage>,
    ) -> Result<Result<RelationOutput, RelationError>, IndexError> {
        if delta.upserts.is_empty() && delta.removed.is_empty() {
            // Nothing in this project changed; the rows of the earlier
            // generations remain valid.
            return Ok(Ok(RelationOutput::default()));
        }
        let pin = GenerationPin {
            view: ctx.view,
            generation,
        };
        let hashes = files_map(conn, pin).await?;
        if hashes.len() > link.max_files() {
            return Ok(Err(RelationError(format!(
                "project `{}` has {} files, above the link stage's bound of {}; contracts were not linked",
                ctx.project_name,
                hashes.len(),
                link.max_files()
            ))));
        }
        let mut texts: BTreeMap<ContentHash, Arc<str>> = {
            let cache = self.cache();
            cache
                .get((ctx.view, generation))
                .map(|a| {
                    a.analysed
                        .values()
                        .map(|f| (f.content_hash, Arc::clone(&f.text)))
                        .collect()
                })
                .unwrap_or_default()
        };
        self.fill_texts(conn, ctx, hashes.values().copied(), &mut texts)
            .await?;
        let files: Vec<(RepoPath, Arc<str>)> = hashes
            .iter()
            .filter_map(|(path, hash)| texts.get(hash).map(|t| (path.clone(), Arc::clone(t))))
            .collect();
        drop(texts);
        let others = self.workspace_extractions(conn, ctx, link).await?;
        let stage = Arc::clone(link);
        let project_name = ctx.project_name.clone();
        let policy = ctx.policy.clone();
        let linked = tokio::task::spawn_blocking(move || {
            let own = stage.extract(&project_name, &files, &policy);
            let mut projects: Vec<ProjectExtractions> = vec![own.clone()];
            projects.extend(others.iter().map(|o| (**o).clone()));
            stage.link(&projects).map(|output| (own, output))
        })
        .await?;
        let (own, output) = match linked {
            Ok(linked) => linked,
            Err(error) => return Ok(Err(error)),
        };
        // Symbol ids of the files that have link sources or extractions.
        let mut source_paths: BTreeSet<RepoPath> =
            own.extractions.iter().map(|e| e.path.clone()).collect();
        source_paths.retain(|p| hashes.contains_key(p));
        let source_paths: Vec<RepoPath> = source_paths.into_iter().collect();
        let mut definitions = Vec::new();
        for batch in source_paths.chunks(PATH_BATCH) {
            definitions.extend(symbols::definitions_in_paths(conn, pin, batch).await?);
        }
        let symbol_index = SymbolIndex::from_definitions(&definitions);
        let cx = RowContext {
            project: ctx.project,
            project_name: &ctx.project_name,
            workspace: ctx.workspace,
            hashes: &hashes,
            symbols: &symbol_index,
        };
        let new_rows = rows(&output, &own, &cx);
        if new_rows.skipped > 0 {
            tracing::debug!(project = %ctx.project_name, skipped = new_rows.skipped, "link rows without a store representation were skipped");
        }
        let universe: BTreeSet<String> = hashes
            .keys()
            .chain(delta.removed.iter())
            .map(origin)
            .collect();
        let origins: Vec<String> = universe.iter().cloned().collect();
        let old_edges: Vec<_> = graph::edges_with_origins(conn, pin, &origins)
            .await?
            .into_iter()
            .map(|e| e.edge)
            .collect();
        let old_contracts: Vec<_> = graph::contracts_with_origins(conn, pin, &origins)
            .await?
            .into_iter()
            .map(|c| c.contract)
            .collect();
        let relation = changed_origins(&universe, new_rows, &old_edges, &old_contracts);
        validate_output(crate::link::LINK_STAGE_NAME, &relation)
            .map_err(|e| IndexError::Inconsistent(e.to_string()))?;
        link.remember(
            ctx.view,
            CachedExtractions {
                generation,
                workspace: ctx.workspace,
                extractions: Arc::new(own),
            },
        );
        Ok(Ok(relation))
    }

    /// T3: relation stage and staleness set, then activation behind the
    /// fence; T2 is queued after activation.
    pub(crate) async fn stage_relations(
        self: &Arc<Self>,
        p: StagePayload,
        run: &JobRun,
    ) -> Result<JobOutcome, IndexError> {
        let ctx = self.context(p.view)?;
        let generation = p
            .generation
            .ok_or_else(|| IndexError::invalid("job payload", "the stage needs a generation"))?;
        let _guard = self.view_lock(p.view).await;
        let mut conn = self.store.acquire().await?;
        if let Some(info) = views::get_generation(&mut conn, p.view, generation).await?
            && info.state == GenerationState::Active
        {
            // Activated by an earlier attempt whose reply was lost.
            self.after_activation(
                &mut conn,
                &ctx,
                &p,
                generation,
                None,
                TierState::Done,
                Vec::new(),
            )
            .await?;
            return Ok(JobOutcome::Completed);
        }
        let active = match self.check_building(&mut conn, &ctx, &p, generation).await? {
            Ok(active) => active,
            Err(reason) => return Ok(JobOutcome::Superseded(reason)),
        };
        self.set_tier(&ctx, generation, &p.target, Tier::T3, TierState::Running);
        let delta = derive_delta(&mut conn, p.view, generation, active).await?;
        let output = if let Some(link) = &self.link {
            Some(
                self.link_relations(&mut conn, &ctx, generation, &delta, link)
                    .await?,
            )
        } else if self.relations_enabled {
            Some(
                self.custom_relations(&mut conn, &ctx, generation, &delta, run)
                    .await?,
            )
        } else {
            None
        };
        let mut t3 = TierState::Done;
        match output {
            Some(Ok(output)) if !output.origins.is_empty() => {
                graph::replace_edges(
                    &mut conn,
                    p.view,
                    generation,
                    &output.origins,
                    &output.edges,
                )
                .await?;
                graph::replace_contracts(
                    &mut conn,
                    p.view,
                    generation,
                    &output.origins,
                    &output.contracts,
                )
                .await?;
                Counters::add(
                    &self.stats.relation_rows_written,
                    (output.edges.len() + output.contracts.len()) as u64,
                );
            }
            Some(Ok(_)) | None => {}
            Some(Err(error)) => {
                let name = if self.link.is_some() {
                    // The link rows of changed and removed files describe
                    // content that is gone: drop them rather than present
                    // them as current. Rows of unchanged files stay.
                    let stale: Vec<String> = delta
                        .upserts
                        .iter()
                        .map(|u| &u.path)
                        .chain(delta.removed.iter())
                        .map(origin)
                        .collect();
                    if !stale.is_empty() {
                        graph::replace_edges(&mut conn, p.view, generation, &stale, &[]).await?;
                        graph::replace_contracts(&mut conn, p.view, generation, &stale, &[])
                            .await?;
                    }
                    crate::link::LINK_STAGE_NAME
                } else {
                    self.relations.name()
                };
                t3 = TierState::Failed {
                    reason: format!("relation stage `{name}`: {error}"),
                };
            }
        }
        run.check()?;
        match views::activate_generation(&mut conn, p.view, generation).await {
            Ok(activation) => {
                self.after_activation(
                    &mut conn,
                    &ctx,
                    &p,
                    generation,
                    activation.previous,
                    t3,
                    delta.stale,
                )
                .await?;
                Ok(JobOutcome::Completed)
            }
            Err(
                error @ (StoreError::StaleGeneration { .. }
                | StoreError::GenerationNotBuilding { .. }),
            ) => Ok(JobOutcome::Superseded(error.to_string())),
            Err(error) => Err(error.into()),
        }
    }

    /// Everything that follows a successful activation: the lexical switch
    /// and garbage collection, history retention, runtime state, events, the
    /// staleness signal, and the T2 job (embeddings are enrichment of an
    /// already searchable generation).
    #[allow(clippy::too_many_arguments)]
    async fn after_activation(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        p: &StagePayload,
        generation: i64,
        previous: Option<i64>,
        t3: TierState,
        stale: Vec<StaleFile>,
    ) -> Result<(), IndexError> {
        let pin = GenerationPin {
            view: ctx.view,
            generation,
        };
        if self.lexical.get(ctx.view, generation)?.is_none() {
            self.rebuild_lexical(conn, ctx, pin).await?;
            self.lexical.get(ctx.view, generation)?;
        }
        let keep = self.lexical_keep(ctx.view, generation);
        self.lexical.collect_garbage(ctx.view, &keep);
        manifest::retain(&self.config.data_dir, ctx.view, &[generation]);
        if let Some(history) = self.config.retention.history_generations {
            let keep_from = generation.saturating_sub(i64::from(history));
            if keep_from >= 1 {
                views::prune_history(conn, ctx.view, keep_from).await?;
            }
        }
        // T2 reuses the analysis of this build; other decisions need none.
        if !matches!(ctx.embedding, EmbeddingDecision::Embed { .. }) {
            self.cache().forget((ctx.view, generation));
        }
        Counters::add(&self.stats.generations_activated, 1);
        let row = views::get_view(conn, ctx.view).await?;
        let current = row.as_ref().is_some_and(|r| {
            p.target.commit().is_none() || r.latest_seen_commit.as_deref() == p.target.commit()
        });
        self.with_runtime(ctx.view, |rt| {
            rt.observed = true;
            rt.last_error = None;
            if current {
                rt.behind_since = None;
            }
        });
        self.set_tier(ctx, generation, &p.target, Tier::T0, TierState::Done);
        self.set_tier(ctx, generation, &p.target, Tier::T1, TierState::Done);
        self.set_tier(ctx, generation, &p.target, Tier::T3, t3);
        let queued = self.enqueue_next(conn, Tier::T2, p, generation).await?;
        if queued.created {
            self.set_tier(ctx, generation, &p.target, Tier::T2, TierState::Pending);
        }
        self.emit(
            ctx,
            Some(generation),
            Some(&p.target),
            ProgressKind::Activated { previous },
        );
        if !stale.is_empty()
            && let Some(sender) = &self.staleness
        {
            let mut files = stale;
            files.sort();
            // A dropped receiver only means nobody tracks staleness.
            let _ = sender.send(StalenessEvent {
                project: ctx.project,
                project_name: ctx.project_name.clone(),
                view: ctx.view,
                generation,
                previous_generation: previous,
                files,
            });
        }
        Ok(())
    }

    /// Lexical generations to keep: the active one and the configured number
    /// of complete older ones.
    fn lexical_keep(&self, view: ViewId, active: i64) -> Vec<i64> {
        let mut older: Vec<i64> = std::fs::read_dir(self.lexical.view_dir(view))
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<i64>().ok()))
                    .filter(|g| *g < active && self.lexical.is_complete(view, *g))
                    .collect()
            })
            .unwrap_or_default();
        older.sort_unstable_by(|a, b| b.cmp(a));
        older.truncate(self.config.retention.lexical_previous);
        older.push(active);
        older
    }
}
