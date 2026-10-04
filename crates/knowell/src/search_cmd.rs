//! `know search`: query the selected local workspace's existing real index.
//!
//! Registration and input failures stay explicit. This command never refreshes
//! sources or substitutes another project, ref or embedding profile.

use std::process::ExitCode;

use clap::Args;
use knowell_core::Name;
use knowell_index::RegistrationIssue;
use knowell_mcp::tools::{SearchInput, SearchOutput};
use knowell_mcp::{Evidence, GapReason, KnowellTools, Target, Validate};
use serde::Serialize;

use crate::db;
use crate::env::Env;
use crate::local_engine::{self, LocalArgs, Prepared};
use crate::output::Output;

#[derive(Debug, Args)]
pub(crate) struct SearchArgs {
    #[command(flatten)]
    local: LocalArgs,
    /// Query, symbol, path or error message; quote queries containing spaces.
    query: String,
    /// Search only this project (repeatable).
    #[arg(long = "project", value_name = "NAME")]
    projects: Vec<Name>,
    /// Maximum results per source or memory search (1–100).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=100))]
    limit: u32,
    /// Omit source snippets from the result.
    #[arg(long)]
    no_snippets: bool,
    /// Include measured retrieval counters and query embedding usage.
    #[arg(long)]
    diagnostics: bool,
    /// Print results, versioned evidence and gaps as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct SearchReport {
    workspace: Name,
    registration_issues: Vec<RegistrationIssue>,
    #[serde(flatten)]
    search: SearchOutput,
}

/// Queries the existing index; returns 1 when the requested index or ref is missing.
pub(crate) fn run(args: SearchArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let prepared = Prepared::load(env, args.local.organization)?;
    prepared.validate_projects(&args.projects)?;
    let input = SearchInput {
        target: Target::workspace(prepared.workspace.name.clone(), Vec::new()),
        query: args.query,
        projects: args.projects,
        limit: Some(args.limit),
        include_snippets: Some(!args.no_snippets),
        include_diagnostics: Some(args.diagnostics),
        ..SearchInput::default()
    };
    // Direct engine calls do not pass through the MCP server's validator.
    input
        .validate()
        .map_err(|error| anyhow::anyhow!(error.client_message("know-search")))?;
    let report = db::runtime()?.block_on(async {
        let local = prepared.open(env).await?;
        let report = async {
            local.require_registered(&input.projects)?;
            // The MCP scope reports missing indexes as gaps, but local commands
            // must not mistake a failed status read for an absent index.
            for registered in &local.registration.views {
                if input.projects.is_empty() || input.projects.contains(&registered.project) {
                    local.status(registered).await?;
                }
            }
            let search = local
                .engine
                .search(&local_engine::caller(), input)
                .await
                .map_err(|error| anyhow::anyhow!(error.client_message("know-search")))?;
            Ok::<_, anyhow::Error>(SearchReport {
                workspace: local.workspace.name.clone(),
                registration_issues: local.registration.issues.clone(),
                search,
            })
        }
        .await;
        local.store().close().await;
        report
    })?;
    let missing_index = has_missing_index(&report.search);
    if args.json {
        out.line(serde_json::to_string_pretty(&report)?)?;
    } else {
        write_report(&report, out)?;
    }
    out.flush()?;
    Ok(if missing_index {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn has_missing_index(search: &SearchOutput) -> bool {
    search.gaps.iter().any(|gap| {
        matches!(
            gap.reason,
            GapReason::ProjectNotIndexed | GapReason::RefNotFound
        )
    })
}

fn write_report(report: &SearchReport, out: &mut Output) -> anyhow::Result<()> {
    out.line(format!(
        "workspace `{}`: {} source hit(s), {} memory hit(s)",
        report.workspace,
        report.search.hits.len(),
        report.search.memory_hits.len(),
    ))?;
    for hit in &report.search.hits {
        out.line(format!("  {}", local_engine::terminal_text(&hit.title)))?;
        write_evidence(&hit.evidence, out)?;
        if !hit.evidence.why.is_empty() {
            out.line(format!(
                "    why: {}",
                local_engine::terminal_text(&knowell_mcp::match_reasons(&hit.evidence.why))
            ))?;
        }
        if let Some(snippet) = &hit.snippet {
            if let Some(lines) = hit.snippet_lines {
                out.line(format!(
                    "    shown: {lines}{}",
                    if hit.snippet_truncated {
                        "; truncated; fetch the result id for the full range"
                    } else {
                        ""
                    }
                ))?;
            }
            out.line(format!(
                "    untrusted source{}:",
                if !snippet.instruction_like().is_empty() {
                    " (instruction-like text detected)"
                } else {
                    ""
                }
            ))?;
            for line in snippet.text().lines() {
                out.line(format!("      {}", local_engine::terminal_text(line)))?;
            }
        }
    }
    for hit in &report.search.memory_hits {
        out.line(format!(
            "  memory {} version {}: {}",
            hit.record.id,
            hit.record.version,
            local_engine::terminal_text(&hit.record.title),
        ))?;
        for evidence in &hit.record.evidence {
            write_evidence(evidence, out)?;
        }
        out.line("    untrusted memory:")?;
        for line in hit.record.body.text().lines() {
            out.line(format!("      {}", local_engine::terminal_text(line)))?;
        }
    }
    for gap in &report.search.gaps {
        out.line(format!(
            "  gap[{}]{}: {}",
            gap.reason.as_str(),
            gap.project
                .as_ref()
                .map_or_else(String::new, |project| format!(" project `{project}`")),
            local_engine::terminal_text(&gap.message),
        ))?;
    }
    for issue in &report.registration_issues {
        out.line(format!(
            "  project `{}` could not be registered: {}",
            issue.project,
            local_engine::terminal_text(&issue.reason),
        ))?;
    }
    if report.search.more_available {
        out.line("  more results are available; increase `--limit`")?;
    }
    Ok(())
}

fn write_evidence(evidence: &Evidence, out: &mut Output) -> anyhow::Result<()> {
    out.line(format!(
        "    {}:{}:{}-{}; view {}; commit {}",
        evidence.project,
        local_engine::terminal_text(evidence.path.as_str()),
        evidence.lines.start(),
        evidence.lines.end(),
        local_engine::terminal_text(&evidence.view.to_string()),
        evidence.commit,
    ))?;
    out.line(format!(
        "    hash {}; freshness {}; index {}",
        evidence.content_hash,
        evidence.freshness.as_str(),
        evidence.index_state.as_str(),
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use knowell_mcp::Gap;
    use knowell_mcp::tools::QueryClass;

    use super::*;

    #[test]
    fn absent_index_or_ref_is_unsuccessful_but_semantic_gaps_remain_information() {
        for reason in [
            GapReason::ProjectNotIndexed,
            GapReason::RefNotFound,
            GapReason::EmbeddingsNotReady,
            GapReason::RelationsNotReady,
            GapReason::NoMatches,
        ] {
            let output = SearchOutput {
                query_class: QueryClass::ExactSymbol,
                diagnostics: None,
                budget: None,
                hits: Vec::new(),
                memory_hits: Vec::new(),
                more_available: false,
                gaps: vec![Gap::new(reason, "synthetic gap")],
            };
            assert_eq!(
                has_missing_index(&output),
                matches!(
                    reason,
                    GapReason::ProjectNotIndexed | GapReason::RefNotFound
                ),
            );
        }
    }
}
