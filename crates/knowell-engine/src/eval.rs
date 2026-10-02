//! The evaluation hook: the engine's hybrid search as a
//! [`knowell_eval::Retriever`] named `hybrid`, so `know eval run` can
//! measure it against grep and BM25 on the same corpus.
//!
//! Relevance in the evaluation set is judged per file, so results are
//! reduced to file ids `<project>/<path>` (the best-ranked chunk of a file
//! decides its rank). The manifest is pinned once when the retriever is
//! created, so every query of a run sees the same generations.

use std::collections::BTreeSet;
use std::sync::Arc;

use knowell_core::Name;
use knowell_eval::{EvalError, RankedDoc, Retriever};
use tokio::runtime::{Handle, RuntimeFlavor};

use crate::access::Access;
use crate::engine::Engine;
use crate::error::EngineError;
use crate::scope::Pinned;
use crate::search::Filters;

/// Name of the retriever in reports.
pub const HYBRID_RETRIEVER: &str = "hybrid";

/// The engine's hybrid search (exact + BM25 + vectors, fused) as an
/// evaluation retriever. Graph expansion is off: expanded items are not
/// ranked results.
pub struct HybridRetriever {
    engine: Engine,
    pinned: Arc<Pinned>,
    handle: Handle,
}

impl std::fmt::Debug for HybridRetriever {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridRetriever")
            .field("workspace", &self.pinned.workspace.name)
            .finish_non_exhaustive()
    }
}

impl HybridRetriever {
    /// Pins `workspace` for `access` and captures the current runtime.
    ///
    /// The retriever trait is synchronous; each search blocks on this
    /// runtime, which must therefore be a multi-thread runtime (calling it
    /// from inside a current-thread runtime would deadlock, so that is
    /// refused here).
    ///
    /// # Errors
    /// [`EngineError::Config`] on a current-thread runtime or outside any
    /// runtime; pinning errors (unknown or invisible workspace).
    pub async fn new(
        engine: &Engine,
        access: &Access,
        workspace: &Name,
    ) -> Result<Self, EngineError> {
        let handle = Handle::try_current().map_err(|_| {
            EngineError::Config("the hybrid retriever needs a tokio runtime".to_owned())
        })?;
        if handle.runtime_flavor() != RuntimeFlavor::MultiThread {
            return Err(EngineError::Config(
                "the hybrid retriever needs a multi-thread tokio runtime".to_owned(),
            ));
        }
        let pinned = engine
            .pin(access, Some(workspace), &[])
            .await
            .map_err(|e| EngineError::NotFound(e.to_string()))?;
        Ok(Self {
            engine: engine.clone(),
            pinned: Arc::new(pinned),
            handle,
        })
    }

    async fn ranked(&self, query: &str, k: usize) -> Result<Vec<RankedDoc>, String> {
        // Ask for more chunks than files: several chunks of one file collapse.
        let limit = k.saturating_mul(3).clamp(10, 300);
        let run = self
            .engine
            .run_search(
                &self.pinned,
                &Filters::default(),
                query,
                limit,
                false,
                false,
            )
            .await
            .map_err(|e| e.to_string())?;
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for result in &run.response.results {
            let id = format!("{}/{}", result.location.project, result.location.path);
            if !seen.insert(id.clone()) {
                continue;
            }
            out.push(RankedDoc {
                id,
                score: result.score.fused,
            });
            if out.len() >= k {
                break;
            }
        }
        Ok(out)
    }
}

impl Retriever for HybridRetriever {
    fn name(&self) -> &str {
        HYBRID_RETRIEVER
    }

    fn search(&self, query: &str, k: usize) -> Result<Vec<RankedDoc>, EvalError> {
        let run = || self.handle.block_on(self.ranked(query, k));
        let result = match Handle::try_current() {
            // Inside a worker thread of the (multi-thread) runtime: hand the
            // thread over while blocking.
            Ok(current) if current.runtime_flavor() == RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(run)
            }
            Ok(_) => {
                return Err(EvalError::Retriever {
                    retriever: HYBRID_RETRIEVER.to_owned(),
                    message: "called from a current-thread runtime; use a multi-thread runtime"
                        .to_owned(),
                });
            }
            Err(_) => run(),
        };
        result.map_err(|message| EvalError::Retriever {
            retriever: HYBRID_RETRIEVER.to_owned(),
            message,
        })
    }
}
