//! `know index`: run one local workspace's durable indexing jobs to idle.
//!
//! Idle is not a completion guarantee: retry delays, superseded builds and
//! failed embedding tiers remain explicit and make the command exit with 1.

use std::process::ExitCode;

use clap::Args;
use knowell_core::Name;
use knowell_index::{BuildTarget, Priority, RegistrationIssue, RunSummary, SyncOutcome};
use serde::Serialize;

use crate::db;
use crate::env::Env;
use crate::local_engine::{self, LocalArgs, LocalEngine, Prepared, ProjectStatus};
use crate::output::Output;

#[derive(Debug, Args)]
pub(crate) struct IndexArgs {
    #[command(flatten)]
    local: LocalArgs,
    /// Rebuild unchanged sources with the current policy and embedding profile;
    /// configured cloud providers may incur charges.
    #[arg(long)]
    rebuild: bool,
    /// Print indexing outcomes and freshness as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct IndexedProject {
    #[serde(flatten)]
    status: ProjectStatus,
    sync: SyncOutcome,
    complete: bool,
    incomplete_reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct IndexReport {
    workspace: Name,
    complete: bool,
    run: RunSummary,
    issues: Vec<RegistrationIssue>,
    projects: Vec<IndexedProject>,
    incomplete_reasons: Vec<String>,
}

/// Runs indexing once; returns 1 for an incomplete target and errors for failed operations.
pub(crate) fn run(args: IndexArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let prepared = Prepared::load(env, args.local.organization)?;
    let report = db::runtime()?.block_on(async {
        let local = prepared.open(env).await?;
        let report = index(&local, args.rebuild).await;
        local.store().close().await;
        report
    })?;
    if args.json {
        out.line(serde_json::to_string_pretty(&report)?)?;
    } else {
        out.line(format!(
            "workspace `{}`: indexing {}",
            report.workspace,
            if report.complete {
                "complete"
            } else {
                "incomplete"
            }
        ))?;
        out.line(format!(
            "  {} job(s) run; {} succeeded; {} failed",
            report.run.jobs, report.run.succeeded, report.run.failed
        ))?;
        for issue in &report.issues {
            out.line(format!(
                "  project `{}` could not be registered: {}",
                issue.project,
                local_engine::terminal_text(&issue.reason)
            ))?;
        }
        for project in &report.projects {
            crate::status_cmd::write_project(&project.status, out)?;
            for reason in &project.incomplete_reasons {
                out.line(format!("    {}", local_engine::terminal_text(reason)))?;
            }
        }
        for reason in &report.incomplete_reasons {
            out.line(format!("  {}", local_engine::terminal_text(reason)))?;
        }
        if !report.complete {
            out.line(
                "inspect `know status`; run `know index` again when the reported cause is resolved",
            )?;
        }
    }
    out.flush()?;
    Ok(if report.complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn index(local: &LocalEngine, rebuild: bool) -> anyhow::Result<IndexReport> {
    let mut requested = Vec::with_capacity(local.registration.views.len());
    for registered in &local.registration.views {
        let previous = local.status(registered).await?;
        let sync = if rebuild {
            local
                .engine
                .indexer()
                .rebuild_view(registered.view, Priority::Interactive)
                .await?
        } else {
            local
                .engine
                .indexer()
                .refresh_view(registered.view, Priority::Interactive)
                .await?
        };
        requested.push((registered, previous, sync));
    }
    let run = local
        .engine
        .indexer()
        .run_until_idle_scoped_with(local.engine.indexer().config().concurrency)
        .await?;
    let mut projects = Vec::with_capacity(requested.len());
    for (registered, previous, sync) in requested {
        let status = local.status(registered).await?;
        let reasons = completion_reasons(&status, &previous, &sync);
        projects.push(IndexedProject {
            status,
            sync,
            complete: reasons.is_empty(),
            incomplete_reasons: reasons,
        });
    }
    let mut reasons = Vec::new();
    if projects.is_empty() {
        reasons.push("the workspace has no registered project views".to_owned());
    }
    if run.failed > 0 {
        reasons.push("some indexing jobs failed; retry-delayed jobs may remain queued".to_owned());
    }
    let issues = local.registration.issues.clone();
    let complete =
        issues.is_empty() && reasons.is_empty() && projects.iter().all(|project| project.complete);
    Ok(IndexReport {
        workspace: local.workspace.name.clone(),
        complete,
        run,
        issues,
        projects,
        incomplete_reasons: reasons,
    })
}

fn completion_reasons(
    project: &ProjectStatus,
    previous: &ProjectStatus,
    sync: &SyncOutcome,
) -> Vec<String> {
    let mut reasons = local_engine::incomplete_reasons(project);
    if reasons
        .iter()
        .any(|reason| reason == "embedding coverage of the active generation is incomplete")
    {
        reasons.push(
            "if the embedding provider, model or dimensions changed, run `know index --rebuild` to build the selected profile; configured cloud providers may incur charges".to_owned(),
        );
    }
    match sync {
        SyncOutcome::Failed { reason, .. } => {
            reasons.push(format!("target resolution failed: {reason}"))
        }
        SyncOutcome::UpToDate {
            commit: Some(commit),
            ..
        } => {
            if project.status.active_commit.as_ref() != Some(commit) {
                reasons.push("the active index no longer matches the requested commit".to_owned());
            }
        }
        SyncOutcome::UpToDate { commit: None, .. } => {
            if project.active_tree_hash.is_none()
                || project.active_tree_hash != previous.active_tree_hash
            {
                reasons.push(
                    "the active index no longer matches the observed directory tree".to_owned(),
                );
            }
        }
        SyncOutcome::Queued { target, .. } => {
            if project.status.active_generation <= previous.status.active_generation {
                reasons.push("the requested build has not activated a newer generation".to_owned());
            }
            let matches = match target {
                BuildTarget::Commit { id } => project.status.active_commit.as_ref() == Some(id),
                BuildTarget::Tree { hash } => project.active_tree_hash.as_ref() == Some(hash),
            };
            if !matches {
                reasons
                    .push("the active index does not match the requested build target".to_owned());
            }
        }
    }
    reasons
}

#[cfg(test)]
mod tests {
    use knowell_core::ContentHash;
    use knowell_index::{
        EmbeddingCoverage, EmbeddingPlan, TierSkip, TierState, TierStates, ViewStatus,
    };
    use knowell_store::{JobId, ProfileId, ViewId};

    use super::*;

    fn directory(generation: Option<i64>, hash: Option<ContentHash>) -> ProjectStatus {
        ProjectStatus {
            status: ViewStatus {
                view: ViewId(uuid::Uuid::nil()),
                workspace: Name::new("synthetic").unwrap(),
                project: Name::new("one").unwrap(),
                target: "worktree".parse().unwrap(),
                latest_seen_commit: None,
                active_commit: None,
                active_generation: generation,
                building_generation: None,
                tiers: TierStates {
                    t0: TierState::Done,
                    t1: TierState::Done,
                    t2: TierState::Skipped {
                        reason: TierSkip::NoProvider,
                    },
                    t3: TierState::Done,
                },
                lag: None,
                last_error: None,
            },
            embedding: EmbeddingPlan::Skip {
                reason: TierSkip::NoProvider,
            },
            embedding_coverage: None,
            active_tree_hash: hash,
        }
    }

    fn queued(target: BuildTarget) -> SyncOutcome {
        SyncOutcome::Queued {
            view: ViewId(uuid::Uuid::nil()),
            target,
            job: JobId(uuid::Uuid::nil()),
            created: true,
        }
    }

    #[test]
    fn an_old_active_directory_generation_is_not_completion() {
        let old_hash = ContentHash::of(b"old synthetic tree");
        let new_hash = ContentHash::of(b"new synthetic tree");
        let previous = directory(Some(1), Some(old_hash));
        let sync = queued(BuildTarget::Tree { hash: new_hash });
        assert!(!completion_reasons(&previous, &previous, &sync).is_empty());
        let finished = directory(Some(2), Some(new_hash));
        assert!(completion_reasons(&finished, &previous, &sync).is_empty());
        let superseded = directory(Some(3), Some(ContentHash::of(b"different tree")));
        assert!(!completion_reasons(&superseded, &previous, &sync).is_empty());
    }

    #[test]
    fn failed_tiers_and_budget_skips_are_not_success() {
        let hash = ContentHash::of(b"synthetic tree");
        let previous = directory(Some(1), Some(hash));
        let sync = SyncOutcome::UpToDate {
            view: previous.status.view,
            commit: None,
        };
        for state in [
            TierState::Pending,
            TierState::Running,
            TierState::Failed {
                reason: "synthetic outage".to_owned(),
            },
            TierState::Skipped {
                reason: TierSkip::BudgetExhausted,
            },
        ] {
            let mut finished = directory(Some(1), Some(hash));
            finished.status.tiers.t2 = state;
            assert!(!completion_reasons(&finished, &previous, &sync).is_empty());
        }
        assert!(completion_reasons(&previous, &previous, &sync).is_empty());
    }

    #[test]
    fn a_new_generation_of_the_wrong_commit_is_not_completion() {
        let mut previous = directory(Some(1), None);
        previous.status.active_commit = Some("a".repeat(40));
        previous.status.latest_seen_commit = previous.status.active_commit.clone();
        let sync = queued(BuildTarget::Commit { id: "b".repeat(40) });
        let mut finished = directory(Some(2), None);
        finished.status.active_commit = Some("c".repeat(40));
        finished.status.latest_seen_commit = finished.status.active_commit.clone();
        assert!(!completion_reasons(&finished, &previous, &sync).is_empty());
        finished.status.active_commit = Some("b".repeat(40));
        finished.status.latest_seen_commit = finished.status.active_commit.clone();
        assert!(completion_reasons(&finished, &previous, &sync).is_empty());
    }

    #[test]
    fn completed_t2_requires_all_inputs_in_the_selected_generation_and_profile() {
        let hash = ContentHash::of(b"synthetic tree");
        let profile = ProfileId(uuid::Uuid::from_u128(1));
        let mut finished = directory(Some(3), Some(hash));
        finished.status.tiers.t2 = TierState::Done;
        finished.embedding = EmbeddingPlan::Embed {
            provider: Name::new("synthetic-provider").unwrap(),
            profile,
            profile_name: Name::new("synthetic-profile").unwrap(),
            dimensions: 64,
        };
        let full = EmbeddingCoverage {
            view: finished.status.view,
            generation: 3,
            profile,
            inputs: 2,
            embedded: 2,
            complete: true,
        };
        for coverage in [
            None,
            Some(EmbeddingCoverage {
                complete: false,
                ..full
            }),
            Some(EmbeddingCoverage {
                embedded: 1,
                ..full
            }),
            Some(EmbeddingCoverage {
                generation: 2,
                ..full
            }),
            Some(EmbeddingCoverage {
                profile: ProfileId(uuid::Uuid::from_u128(2)),
                ..full
            }),
        ] {
            finished.embedding_coverage = coverage;
            assert!(!local_engine::incomplete_reasons(&finished).is_empty());
        }
        finished.embedding_coverage = Some(full);
        assert!(local_engine::incomplete_reasons(&finished).is_empty());
    }
}
