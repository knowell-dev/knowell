//! `open_workspace` and `index_status`.

use std::collections::{BTreeMap, BTreeSet};

use knowell_index::{TierSkip, TierState as IndexTierState};
use knowell_knowledge::{BootstrapInputs, Tier, bootstrap_pack};
use knowell_mcp::tools::{
    IndexStatusInput, IndexStatusOutput, JobInfo, JobKind, LanguageCoverage, OpenWorkspaceInput,
    OpenWorkspaceOutput, ProjectIndexStatus, ProjectInfo, TierState, TierStatus,
};
use knowell_mcp::{
    AnalysisLevel, CommitId, FreshnessTier, Gap, GapReason, IndexState, JobId, JobState, Timestamp,
    ToolError, ViewLayer,
};
use knowell_parse::Language as ParseLanguage;
use knowell_store::jobs;
use time::format_description::well_known::Rfc3339;

use super::memory::{conflict_map, memory_error, task_summary};
use crate::access::Access;
use crate::engine::Engine;
use crate::error::store_tool;
use crate::tools::memory_record;

/// Analysis depth of a language name as stored (`typescript`, `go`, …).
pub(crate) fn analysis_level(language: &str) -> AnalysisLevel {
    let parsed = ParseLanguage::ALL
        .iter()
        .copied()
        .find(|l| l.as_str() == language);
    match parsed.map(ParseLanguage::tier) {
        Some(knowell_parse::Tier::TextOnly) | None => AnalysisLevel::Text,
        // No SCIP or language tool is integrated yet: tree-sitter only.
        Some(_) => AnalysisLevel::Syntactic,
    }
}

fn tier_state(state: &IndexTierState) -> TierState {
    match state {
        IndexTierState::Pending => TierState::Queued,
        IndexTierState::Running => TierState::Building,
        IndexTierState::Done => TierState::Ready,
        IndexTierState::Skipped {
            reason: TierSkip::NoProvider,
        } => TierState::Disabled,
        IndexTierState::Skipped { .. } | IndexTierState::Failed { .. } => TierState::Unavailable,
    }
}

fn rfc3339(at: time::OffsetDateTime) -> Option<Timestamp> {
    at.format(&Rfc3339)
        .ok()
        .and_then(|t| Timestamp::new(t).ok())
}

fn job_state(state: knowell_store::JobState) -> JobState {
    match state {
        knowell_store::JobState::Queued | knowell_store::JobState::Failed => JobState::Queued,
        knowell_store::JobState::Running => JobState::Running,
        knowell_store::JobState::Succeeded => JobState::Succeeded,
        knowell_store::JobState::Dead => JobState::Failed,
        knowell_store::JobState::Cancelled => JobState::Cancelled,
    }
}

fn job_kind(kind: &str) -> JobKind {
    match kind {
        "index.embeddings" => JobKind::Embed,
        "index.relations" => JobKind::Relations,
        _ => JobKind::Index,
    }
}

impl Engine {
    pub(crate) async fn tool_open_workspace(
        &self,
        access: Access,
        input: OpenWorkspaceInput,
    ) -> Result<OpenWorkspaceOutput, ToolError> {
        let mut pinned = self
            .pin(&access, input.workspace.as_ref(), &input.views)
            .await?;
        let mut gaps = pinned.gaps.clone();
        if let Some(dir) = &input.working_directory {
            gaps.extend(self.attach_worktree(&access, &mut pinned, dir).await);
        }
        gaps.extend(pinned.not_indexed_gaps(&[]));
        for issue in &pinned.workspace.issues {
            gaps.push(Gap::for_project(
                GapReason::ProjectNotIndexed,
                issue.project.clone(),
                format!(
                    "{} could not be registered: {}",
                    issue.project, issue.reason
                ),
            ));
        }
        let mut projects = Vec::new();
        for entry in &pinned.workspace.projects {
            if !access.reads_project(&pinned.workspace.name, &entry.name) {
                continue;
            }
            let languages = match pinned.projects.get(&entry.name) {
                Some(project) => {
                    let snapshot = self.snapshot_of(project).await?;
                    let mut counts: Vec<(String, u64)> = snapshot.languages().into_iter().collect();
                    counts.sort_by(|(la, a), (lb, b)| b.cmp(a).then_with(|| la.cmp(lb)));
                    counts
                        .into_iter()
                        .filter(|(l, _)| l != "text")
                        .take(5)
                        .map(|(l, _)| l)
                        .collect()
                }
                None => Vec::new(),
            };
            projects.push(ProjectInfo {
                name: entry.name.clone(),
                description: None,
                roles: Vec::new(),
                languages,
                root: entry.root.clone(),
                tracks: entry.target.clone(),
            });
        }
        let (records, task_rows) = self.bootstrap_inputs(&access, &pinned).await?;
        let budget = usize::try_from(input.summary_budget_tokens.unwrap_or(2000)).unwrap_or(2000);
        let pack = bootstrap_pack(
            &BootstrapInputs {
                records: records.clone(),
                tasks: task_rows.iter().map(|r| r.task.clone()).collect(),
                project_maps: Vec::new(),
                scope_filter: None,
            },
            budget,
        );
        let conflicts = conflict_map(&records);
        let by_id: BTreeMap<_, _> = records.iter().map(|r| (r.id, r)).collect();
        let mut rules = Vec::new();
        let mut recent_decisions = Vec::new();
        let mut open_tasks = Vec::new();
        for item in &pack.items {
            if let Some(id) = item.source.record
                && let Some(record) = by_id.get(&id)
            {
                let empty = Vec::new();
                let mcp =
                    memory_record(record, conflicts.get(&id).unwrap_or(&empty), Some(&pinned))?;
                match item.tier {
                    Tier::Pinned | Tier::Rule => rules.push(mcp),
                    _ => recent_decisions.push(mcp),
                }
            } else if let Some(task) = item.source.task
                && let Some(row) = task_rows.iter().find(|r| r.task.id == task)
            {
                let last = self
                    .inner
                    .memory
                    .checkpoints(task)
                    .await
                    .map_err(memory_error)?
                    .last()
                    .map(|c| c.seq);
                open_tasks.push(task_summary(row, last)?);
            }
        }
        if !pack.omitted.is_empty() {
            gaps.push(Gap::new(
                GapReason::BudgetExhausted,
                format!(
                    "{} more memory items did not fit summary_budget_tokens; use read_memory or resume_task",
                    pack.omitted.len()
                ),
            ));
        }
        if !pack.conflicts.is_empty() {
            gaps.push(Gap::new(
                GapReason::NoMatches,
                format!(
                    "{} pairs of accepted records contradict each other; see conflicts_with",
                    pack.conflicts.len()
                ),
            ));
        }
        let current_project = pinned.current_project.clone();
        let manifest = pinned.manifest();
        let workspace = pinned.workspace.name.clone();
        let context_id = self.create_context(&access, pinned)?;
        Ok(OpenWorkspaceOutput {
            context_id,
            workspace,
            current_project,
            manifest,
            projects,
            rules,
            open_tasks,
            recent_decisions,
            gaps,
        })
    }

    pub(crate) async fn tool_index_status(
        &self,
        access: Access,
        input: IndexStatusInput,
    ) -> Result<IndexStatusOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        let ws = &pinned.workspace.name;
        for project in &input.projects {
            if pinned.workspace.project(project).is_none() || !access.reads_project(ws, project) {
                return Err(ToolError::not_found(format!(
                    "project {project} does not exist"
                )));
            }
        }
        let wanted =
            |name: &knowell_core::Name| input.projects.is_empty() || input.projects.contains(name);
        let mut out = Vec::new();
        let mut gaps = pinned.gaps.clone();
        for entry in &pinned.workspace.projects {
            if !wanted(&entry.name) || !access.reads_project(ws, &entry.name) {
                continue;
            }
            let status = self.inner.indexer.status(entry.view).await.ok();
            let pinned_project = pinned.projects.get(&entry.name);
            let mut languages = Vec::new();
            if let Some(project) = pinned_project {
                let snapshot = self.snapshot_of(project).await?;
                for (language, files) in snapshot.languages() {
                    languages.push(LanguageCoverage {
                        analysis: analysis_level(&language),
                        language,
                        files,
                    });
                }
            }
            let tiers = status
                .as_ref()
                .map(|s| {
                    [
                        (FreshnessTier::T0Text, &s.tiers.t0),
                        (FreshnessTier::T1Symbols, &s.tiers.t1),
                        (FreshnessTier::T2Embeddings, &s.tiers.t2),
                        (FreshnessTier::T3Relations, &s.tiers.t3),
                    ]
                    .into_iter()
                    .map(|(tier, state)| TierStatus {
                        tier,
                        state: tier_state(state),
                        files_done: None,
                        files_total: None,
                    })
                    .collect()
                })
                .unwrap_or_default();
            let embedding_profile = match &entry.embedding {
                knowell_index::EmbeddingPlan::Embed { profile_name, .. } => {
                    Some(profile_name.to_string())
                }
                _ => None,
            };
            let state = match pinned_project {
                Some(p) => p.index_state(),
                None => IndexState::NotIndexed,
            };
            let message =
                status
                    .as_ref()
                    .and_then(|s| s.last_error.clone())
                    .or_else(|| match &entry.embedding {
                        knowell_index::EmbeddingPlan::Skip { reason } => {
                            Some(format!("embeddings skipped: {}", reason.as_str()))
                        }
                        knowell_index::EmbeddingPlan::Unavailable { reason } => {
                            Some(format!("embeddings unavailable: {reason}"))
                        }
                        knowell_index::EmbeddingPlan::Embed { .. } => None,
                    });
            out.push(ProjectIndexStatus {
                project: entry.name.clone(),
                tracking: entry.target.clone(),
                layer: if pinned_project.is_some_and(|p| p.overlay.is_some()) {
                    ViewLayer::Personal
                } else {
                    ViewLayer::Shared
                },
                latest_seen_commit: status
                    .as_ref()
                    .and_then(|s| s.latest_seen_commit.as_deref())
                    .and_then(|c| CommitId::new(c).ok()),
                indexed_commit: pinned_project.and_then(|p| p.commit_id()),
                state,
                tiers,
                languages,
                embedding_profile,
                last_indexed_at: pinned_project
                    .and_then(|p| p.activated_at)
                    .and_then(rfc3339),
                message,
            });
            if pinned_project.is_none() && !pinned.not_indexed.contains(&entry.name) {
                // Pinned at another ref that is not indexed; the pin gap explains it.
                continue;
            }
        }
        let mut job_infos = Vec::new();
        let mut seen = BTreeSet::new();
        let mut conn = self.inner.store.acquire().await.map_err(store_tool)?;
        for id in &input.job_ids {
            let Ok(uuid) = uuid::Uuid::parse_str(id.as_str()) else {
                gaps.push(Gap::new(
                    GapReason::NotFound,
                    format!("job {id} does not exist"),
                ));
                continue;
            };
            match jobs::get_job(&mut conn, knowell_store::JobId(uuid))
                .await
                .map_err(store_tool)?
            {
                Some(job) => {
                    seen.insert(job.id);
                    job_infos.push(job_info(&job, id.clone()));
                }
                None => gaps.push(Gap::new(
                    GapReason::NotFound,
                    format!("job {id} does not exist"),
                )),
            }
        }
        let mut filter = jobs::JobFilter::new(50);
        filter.states = vec![
            knowell_store::JobState::Running,
            knowell_store::JobState::Queued,
        ];
        filter.scope = jobs::JobScopeFilter::organization(self.inner.organization, true);
        for job in jobs::list_jobs(&mut conn, &filter)
            .await
            .map_err(store_tool)?
        {
            let project = job
                .payload
                .get("view")
                .and_then(|v| v.as_str())
                .and_then(|v| uuid::Uuid::parse_str(v).ok())
                .and_then(|v| {
                    pinned
                        .workspace
                        .projects
                        .iter()
                        .find(|p| p.view.0 == v)
                        .map(|p| p.name.clone())
                });
            // Jobs of projects the caller cannot see are not shown.
            let Some(project) = project else { continue };
            if !access.reads_project(ws, &project) || !seen.insert(job.id) {
                continue;
            }
            if let Ok(id) = JobId::new(job.id.to_string()) {
                let mut info = job_info(&job, id);
                info.project = Some(project);
                job_infos.push(info);
            }
        }
        Ok(IndexStatusOutput {
            projects: out,
            jobs: job_infos,
            gaps,
        })
    }
}

fn job_info(job: &jobs::Job, id: JobId) -> JobInfo {
    JobInfo {
        job_id: id,
        kind: job_kind(&job.kind),
        state: job_state(job.state),
        progress_percent: None,
        project: None,
        started_at: job.started_at.and_then(rfc3339),
        finished_at: job.finished_at.and_then(rfc3339),
        message: job.last_error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analysis_levels_follow_the_language_tier() {
        assert_eq!(analysis_level("typescript"), AnalysisLevel::Syntactic);
        assert_eq!(analysis_level("dart"), AnalysisLevel::Syntactic);
        assert_eq!(analysis_level("html"), AnalysisLevel::Text);
        assert_eq!(analysis_level("klingon"), AnalysisLevel::Text);
    }

    #[test]
    fn tier_states_map() {
        assert_eq!(tier_state(&IndexTierState::Done), TierState::Ready);
        assert_eq!(tier_state(&IndexTierState::Running), TierState::Building);
        assert_eq!(
            tier_state(&IndexTierState::Skipped {
                reason: TierSkip::NoProvider
            }),
            TierState::Disabled
        );
        assert_eq!(
            tier_state(&IndexTierState::Skipped {
                reason: TierSkip::DataPolicyLocalOnly
            }),
            TierState::Unavailable
        );
    }
}
