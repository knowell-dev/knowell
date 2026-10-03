//! Read-only local task commands backed by the engine's scoped resume tool.

use std::process::ExitCode;

use clap::{Args, Subcommand};
use knowell_core::Name;
use knowell_index::RegistrationIssue;
use knowell_mcp::tools::{
    ChangeKind, ResumeTaskInput, ResumeTaskOutput, SourceChange, TaskDetail, TaskStatus,
    TaskSummary,
};
use knowell_mcp::{Gap, GapReason, KnowellTools, ProjectView, Target, TaskId, ToolError, Validate};
use serde::Serialize;

use crate::db;
use crate::env::Env;
use crate::graph_output::{GraphFormat, GraphOutputArgs};
use crate::local_engine::{self, LocalArgs, Prepared};
use crate::memory_cmd::{
    author_text, render_gaps, render_record, render_registration, render_untrusted, safe, section,
};
use crate::output::Output;

#[derive(Debug, Subcommand)]
pub(crate) enum TaskCommand {
    /// List readable in-progress and blocked tasks in the local workspace.
    List(TaskListArgs),
    /// Show one readable task and its newest saved checkpoints.
    Show(TaskShowArgs),
}

#[derive(Debug, Args)]
pub(crate) struct TaskListArgs {
    #[command(flatten)]
    local: LocalArgs,
    #[command(flatten)]
    output: GraphOutputArgs,
    /// Maximum tasks (1–100).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=100))]
    limit: u32,
}

#[derive(Debug, Args)]
pub(crate) struct TaskShowArgs {
    #[command(flatten)]
    local: LocalArgs,
    #[command(flatten)]
    output: GraphOutputArgs,
    /// Task id returned by a task read or checkpoint save.
    #[arg(value_name = "TASK_ID")]
    id: String,
    /// Maximum newest checkpoints (1–100).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=100))]
    limit: u32,
}

#[derive(Debug, Serialize)]
struct TaskReport {
    workspace: Name,
    registration_issues: Vec<RegistrationIssue>,
    #[serde(flatten)]
    task: ResumeTaskOutput,
}

/// Reads tasks without indexing or preparing any provider client.
pub(crate) fn run(command: TaskCommand, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let (local_args, output, id, limit) = match command {
        TaskCommand::List(args) => (args.local, args.output, None, args.limit),
        TaskCommand::Show(args) => (
            args.local,
            args.output,
            Some(parse_id(&args.id)?),
            args.limit,
        ),
    };
    let show = id.is_some();
    let prepared = Prepared::load_records(env, local_args.organization)?;
    let input = ResumeTaskInput {
        target: Target::workspace(prepared.workspace.name.clone(), Vec::new()),
        task_id: id,
        limit: Some(limit),
        ..ResumeTaskInput::default()
    };
    input
        .validate()
        .map_err(|error| anyhow::anyhow!(error.client_message("know-task")))?;
    let report = db::runtime()?.block_on(async {
        let local = prepared.open_records(env).await?;
        let result = async {
            local.require_registered(&[])?;
            // Source gaps are informative; failed store reads remain errors.
            for registered in &local.registration.views {
                local.engine.indexer().status(registered.view).await?;
            }
            let task = match local
                .engine
                .resume_task(&local_engine::caller(), input)
                .await
            {
                Ok(task) => task,
                Err(ToolError::NotFound(_)) if show => ResumeTaskOutput {
                    gaps: vec![Gap::new(
                        GapReason::NotFound,
                        "the task does not exist in the readable scopes",
                    )],
                    ..ResumeTaskOutput::default()
                },
                Err(error) => return Err(anyhow::anyhow!(error.client_message("know-task"))),
            };
            Ok::<_, anyhow::Error>(TaskReport {
                workspace: local.workspace.name.clone(),
                registration_issues: local.registration.issues.clone(),
                task,
            })
        }
        .await;
        local.store().close().await;
        result
    })?;
    let body = match output.format() {
        GraphFormat::Json => serde_json::to_string_pretty(&report)?,
        format => render_report(&report, format),
    };
    output.emit(&body, out)?;
    Ok(if show && report.task.task.is_none() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn parse_id(value: &str) -> anyhow::Result<TaskId> {
    let id = TaskId::new(value).map_err(|_| anyhow::anyhow!("the task id is invalid"))?;
    uuid::Uuid::parse_str(id.as_str())
        .map_err(|_| anyhow::anyhow!("the task id must be a task UUID"))?;
    Ok(id)
}

fn render_report(report: &TaskReport, format: GraphFormat) -> String {
    let markdown = matches!(format, GraphFormat::Markdown);
    let mut lines = Vec::new();
    if markdown {
        lines.push("## Knowell tasks".to_owned());
        lines.push(String::new());
    }
    lines.push(format!(
        "Workspace: {}",
        safe(report.workspace.as_str(), markdown)
    ));
    if let Some(task) = &report.task.task {
        render_detail(task, markdown, &mut lines);
    } else {
        lines.push(format!("{} task(s).", report.task.tasks.len()));
        for task in &report.task.tasks {
            render_summary(task, markdown, &mut lines);
        }
    }
    render_gaps(&report.task.gaps, markdown, &mut lines);
    render_registration(&report.registration_issues, markdown, &mut lines);
    lines.join("\n")
}

fn render_summary(task: &TaskSummary, markdown: bool, lines: &mut Vec<String>) {
    section(&format!("Task {}", task.task_id), markdown, lines);
    lines.push(format!(
        "Title (untrusted): {}",
        safe(&task.title, markdown)
    ));
    lines.push(format!(
        "Status: {}; updated: {}",
        status_name(task.status),
        task.updated_at
    ));
    lines.push(format!("Owner: {}", author_text(&task.owner, markdown)));
    if let Some(checkpoint) = &task.last_checkpoint {
        lines.push(format!(
            "Latest checkpoint: {}",
            safe(checkpoint.as_str(), markdown)
        ));
    }
    render_untrusted("Goal", &task.goal, markdown, lines);
}

fn render_detail(task: &TaskDetail, markdown: bool, lines: &mut Vec<String>) {
    render_summary(&task.summary, markdown, lines);
    section("Saved checkpoints (newest first)", markdown, lines);
    if task.checkpoints.is_empty() {
        lines.push("No checkpoints are reported.".to_owned());
    }
    for checkpoint in &task.checkpoints {
        lines.push(format!(
            "Checkpoint {} (sequence {}), saved {} by {}",
            safe(checkpoint.checkpoint_id.as_str(), markdown),
            checkpoint.sequence,
            checkpoint.saved_at,
            author_text(&checkpoint.author, markdown),
        ));
        render_untrusted("Progress", &checkpoint.progress, markdown, lines);
    }
    section("Decisions", markdown, lines);
    if task.decisions.is_empty() {
        lines.push("No decisions are reported.".to_owned());
    }
    for decision in &task.decisions {
        render_record(decision, markdown, true, lines);
    }
    if !task.open_questions.is_empty() {
        section("Open questions", markdown, lines);
        for question in &task.open_questions {
            render_untrusted("Question", question, markdown, lines);
        }
    }
    if !task.next_steps.is_empty() {
        section("Next steps", markdown, lines);
        for step in &task.next_steps {
            render_untrusted("Step", step, markdown, lines);
        }
    }
    if !task.related_symbols.is_empty() {
        section("Related symbols (untrusted)", markdown, lines);
        for symbol in &task.related_symbols {
            lines.push(format!("- {}", safe(symbol, markdown)));
        }
    }
    section("Saved source manifest", markdown, lines);
    if task.manifest.is_empty() {
        lines.push("No saved source manifest is reported.".to_owned());
    }
    for view in &task.manifest {
        render_view(view, markdown, lines);
    }
    if !task.changed_since.is_empty() {
        section("Source changes since the saved manifest", markdown, lines);
        for change in &task.changed_since {
            render_change(change, markdown, lines);
        }
    }
    if !task.stale_knowledge.is_empty() {
        section("Stale knowledge", markdown, lines);
        for record in &task.stale_knowledge {
            render_record(record, markdown, true, lines);
        }
    }
}

fn render_view(view: &ProjectView, markdown: bool, lines: &mut Vec<String>) {
    let commit = view
        .commit
        .as_ref()
        .map_or("none reported", |value| value.as_str());
    let freshness = view
        .freshness
        .map_or("none reported", |value| value.as_str());
    lines.push(format!(
        "- Project {}; view {}; layer {}; saved commit {}; local generation {}; freshness {}; index {}",
        safe(view.project.as_str(), markdown),
        safe(&view.view.to_string(), markdown),
        match view.layer {
            knowell_mcp::ViewLayer::Shared => "shared",
            knowell_mcp::ViewLayer::Personal => "personal",
        },
        safe(commit, markdown),
        view.local_generation,
        freshness,
        view.index_state.as_str(),
    ));
}

fn render_change(change: &SourceChange, markdown: bool, lines: &mut Vec<String>) {
    let previous = change
        .previous_path
        .as_ref()
        .map_or_else(String::new, |path| {
            format!("; previous path {}", safe(path.as_str(), markdown))
        });
    lines.push(format!(
        "- {}:{} {}; from {} to {}{}",
        safe(change.project.as_str(), markdown),
        safe(change.path.as_str(), markdown),
        match change.change {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Renamed => "renamed",
        },
        safe(change.from_commit.as_str(), markdown),
        safe(change.to_commit.as_str(), markdown),
        previous,
    ));
}

fn status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
        TaskStatus::Abandoned => "abandoned",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_task_ids_do_not_echo_rejected_text() {
        for value in [
            "",
            "not-a-uuid",
            "KNOWELL_CANARY_ID\u{001b}\n",
            &"x".repeat(300),
        ] {
            let error = parse_id(value).unwrap_err();
            assert!(!error.to_string().contains("KNOWELL_CANARY"));
            assert!(!error.to_string().chars().any(char::is_control));
        }
    }
}
