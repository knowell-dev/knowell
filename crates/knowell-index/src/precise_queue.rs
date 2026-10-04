//! Durable, explicitly requested compiler-analysis builds.

use knowell_embed::Embedder;
use knowell_store::ViewId;
use knowell_store::analysis;
use knowell_store::views::GenerationPin;
use sqlx::Connection;

use crate::Priority;
use crate::context::ViewContext;
use crate::error::IndexError;
use crate::indexer::{Indexer, Inner, SyncOutcome};
use crate::jobs::StagePayload;
use crate::precise::{PreparedScipImport, ScipImportLimits};

impl<E: Embedder + 'static> Indexer<E> {
    /// Queues a full analysis generation using an explicitly supplied SCIP
    /// artifact. Existing content and vectors remain reusable by their hashes.
    /// The artifact is durable before the job is queued and cannot follow a
    /// different revision or be inherited by later ordinary refreshes.
    ///
    /// This does not run a compiler, external indexer, or worker. Run the
    /// registered worker or `run_until_idle` to finish the build.
    pub async fn rebuild_view_with_scip(
        &self,
        view: ViewId,
        prepared: &PreparedScipImport,
        priority: Priority,
    ) -> Result<SyncOutcome, IndexError> {
        let limits = ScipImportLimits::default();
        let ctx = self.inner.context(view)?;
        let encoded = prepared.to_json(&limits)?;
        let payload = serde_json::from_slice(&encoded)
            .map_err(|_| IndexError::invalid("scip import", "prepared data cannot be encoded"))?;
        let manifest = prepared.manifest();
        let mut conn = self.inner.store.acquire().await?;
        let id = analysis::stage_scip_import(
            &mut conn,
            ctx.organization,
            view,
            &manifest.source_revision,
            &manifest.artifact_hash,
            &payload,
        )
        .await?;
        drop(conn);
        self.inner
            .sync_with_scip(view, priority, true, Some(id))
            .await
    }
}

impl<E: Embedder + 'static> Inner<E> {
    /// Applies only the artifact explicitly attached to this durable build.
    /// Removal and replacement share one fenced transaction before activation.
    pub(crate) async fn apply_staged_scip(
        &self,
        conn: &mut knowell_store::PgConnection,
        ctx: &ViewContext,
        pin: GenerationPin,
        payload: &StagePayload,
    ) -> Result<(), IndexError> {
        let mut transaction = conn
            .begin()
            .await
            .map_err(knowell_store::StoreError::from)?;
        analysis::clear_scip_edges(&mut transaction, ctx.organization, pin).await?;
        let Some(id) = payload.scip_import else {
            transaction
                .commit()
                .await
                .map_err(knowell_store::StoreError::from)?;
            return Ok(());
        };
        let revision = payload
            .target
            .commit()
            .ok_or_else(|| IndexError::invalid("scip import", "a git revision is required"))?;
        let value =
            analysis::load_scip_import(&mut transaction, ctx.organization, pin.view, id, revision)
                .await?
                .ok_or_else(|| {
                    IndexError::invalid("scip import", "the staged artifact is unavailable")
                })?;
        let bytes = serde_json::to_vec(&value)
            .map_err(|_| IndexError::invalid("scip import", "stored data cannot be decoded"))?;
        let prepared = PreparedScipImport::from_json(&bytes, &ScipImportLimits::default())?;
        self.apply_scip_import(&mut transaction, ctx, pin, &prepared)
            .await?;
        transaction
            .commit()
            .await
            .map_err(knowell_store::StoreError::from)?;
        Ok(())
    }
}
