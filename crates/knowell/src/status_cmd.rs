//! `know status`: read the selected local workspace's real freshness and coverage.

use std::process::ExitCode;

use clap::Args;
use knowell_core::Name;
use knowell_index::{EmbeddingPlan, RegistrationIssue, Tier, TierState};
use serde::Serialize;

use crate::db;
use crate::env::Env;
use crate::local_engine::{self, LocalArgs, Prepared, ProjectStatus};
use crate::output::Output;

#[derive(Debug, Args)]
pub(crate) struct StatusArgs {
    #[command(flatten)]
    local: LocalArgs,
    /// Report only this project (repeatable).
    #[arg(long = "project", value_name = "NAME")]
    projects: Vec<Name>,
    /// Print index freshness and embedding coverage as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct StatusReport {
    workspace: Name,
    projects: Vec<ProjectStatus>,
    issues: Vec<RegistrationIssue>,
}

/// Reports saved status; failed tiers are data and failed store reads are errors.
pub(crate) fn run(args: StatusArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let prepared = Prepared::load(env, args.local.organization)?;
    prepared.validate_projects(&args.projects)?;
    let report = db::runtime()?.block_on(async {
        let local = prepared.open(env).await?;
        let report = async {
            let mut projects = Vec::new();
            for registered in &local.registration.views {
                if args.projects.is_empty() || args.projects.contains(&registered.project) {
                    projects.push(local.status(registered).await?);
                }
            }
            Ok::<_, anyhow::Error>(StatusReport {
                workspace: local.workspace.name.clone(),
                projects,
                issues: local
                    .registration
                    .issues
                    .iter()
                    .filter(|issue| {
                        args.projects.is_empty() || args.projects.contains(&issue.project)
                    })
                    .cloned()
                    .collect(),
            })
        }
        .await;
        local.store().close().await;
        report
    })?;
    if args.json {
        out.line(serde_json::to_string_pretty(&report)?)?;
    } else {
        out.line(format!("workspace `{}`", report.workspace))?;
        for project in &report.projects {
            write_project(project, out)?;
        }
        for issue in &report.issues {
            out.line(format!(
                "  project `{}` could not be registered: {}",
                issue.project,
                local_engine::terminal_text(&issue.reason)
            ))?;
        }
        if report.projects.is_empty() && report.issues.is_empty() {
            out.line("  no project views are registered")?;
        }
    }
    out.flush()?;
    // A failed tier is successful status information, not a failed retrieval.
    Ok(ExitCode::SUCCESS)
}

/// Writes one project's active target and tiers without emitting terminal controls.
pub(crate) fn write_project(project: &ProjectStatus, out: &mut Output) -> anyhow::Result<()> {
    let status = &project.status;
    out.line(format!(
        "  project `{}` tracks {}: active generation {}, building generation {}",
        status.project,
        local_engine::terminal_text(&status.target.to_string()),
        status
            .active_generation
            .map_or_else(|| "none".to_owned(), |g| g.to_string()),
        status
            .building_generation
            .map_or_else(|| "none".to_owned(), |g| g.to_string()),
    ))?;
    out.line(format!(
        "    latest seen commit: {}; active commit: {}",
        local_engine::terminal_text(status.latest_seen_commit.as_deref().unwrap_or("none")),
        local_engine::terminal_text(status.active_commit.as_deref().unwrap_or("none")),
    ))?;
    for tier in Tier::ALL {
        let state = match status.tiers.get(tier) {
            TierState::Pending => "pending".to_owned(),
            TierState::Running => "running".to_owned(),
            TierState::Done => "done".to_owned(),
            TierState::Skipped { reason } => format!("skipped ({})", reason.as_str()),
            TierState::Failed { reason } => {
                format!("failed ({})", local_engine::terminal_text(reason))
            }
        };
        out.line(format!("    {}: {state}", tier.as_str()))?;
    }
    if let Some(error) = &status.last_error {
        out.line(format!(
            "    last error: {}",
            local_engine::terminal_text(error)
        ))?;
    }
    if let EmbeddingPlan::Unavailable { reason } = &project.embedding {
        out.line(format!(
            "    embeddings unavailable: {}",
            local_engine::terminal_text(reason)
        ))?;
    }
    if let Some(coverage) = project.embedding_coverage {
        out.line(format!(
            "    embeddings: {}/{} input(s); vector generation {}",
            coverage.embedded,
            coverage.inputs,
            if coverage.complete {
                "complete"
            } else {
                "incomplete"
            },
        ))?;
    }
    if let Some(hash) = project.active_tree_hash {
        out.line(format!("    active directory tree: {hash}"))?;
    }
    Ok(())
}
