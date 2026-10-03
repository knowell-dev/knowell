//! `know trace`: evidenced flows over the selected local workspace's saved graph.
//!
//! This command queries existing generations without starting indexing or
//! provider requests. Missing starts, indexes and refs remain explicit.

use std::process::ExitCode;

use clap::{Args, ValueEnum};
use knowell_core::Name;
use knowell_index::RegistrationIssue;
use knowell_mcp::tools::{FlowDirection, TraceFlowInput, TraceFlowOutput};
use knowell_mcp::{GapReason, KnowellTools, RelationKind, ResultId, Target, Validate};
use serde::Serialize;

use crate::db;
use crate::env::Env;
use crate::graph_output::{self, GraphFormat, GraphOutputArgs};
use crate::local_engine::{self, LocalArgs, Prepared};
use crate::output::Output;

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("trace_start").args(["symbol", "id", "contract"])))]
pub(crate) struct TraceArgs {
    #[command(flatten)]
    local: LocalArgs,
    /// Symbol to trace; quote qualified names containing spaces.
    #[arg(required_unless_present_any = ["id", "contract", "hub", "oidc_audience"])]
    symbol: Option<String>,
    /// Existing source result id to trace instead of a symbol.
    #[arg(long, value_name = "RESULT_ID")]
    id: Option<ResultId>,
    /// Contract key to trace instead of a symbol, for example topic:order.created.
    #[arg(long, value_name = "KEY")]
    contract: Option<String>,
    /// Project defining the start symbol; flows can reach other projects.
    #[arg(long, value_name = "NAME", requires = "symbol", conflicts_with_all = ["id", "contract"])]
    project: Option<Name>,
    /// Direction to follow from the start.
    #[arg(long, value_enum, default_value_t = Direction::Downstream)]
    direction: Direction,
    /// Maximum relation hops (1–5).
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=5))]
    max_depth: u8,
    /// Follow only this MCP relation kind, for example imports or http_call (repeatable).
    #[arg(long = "relation", value_name = "KIND", value_parser = parse_relation, value_delimiter = ',')]
    relations: Vec<RelationKind>,
    /// Maximum nodes across the entire trace (1–200).
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=200))]
    limit: u32,
    /// Hub transport is not supported by this standalone command.
    #[arg(long, alias = "hub-url", value_name = "URL")]
    hub: Option<String>,
    /// Hub OIDC transport is not supported by this standalone command.
    #[arg(long, value_name = "AUDIENCE")]
    oidc_audience: Option<String>,
    #[command(flatten)]
    output: GraphOutputArgs,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Direction {
    Downstream,
    Upstream,
    Both,
}

impl From<Direction> for FlowDirection {
    fn from(direction: Direction) -> Self {
        match direction {
            Direction::Downstream => Self::Downstream,
            Direction::Upstream => Self::Upstream,
            Direction::Both => Self::Both,
        }
    }
}

fn parse_relation(value: &str) -> Result<RelationKind, String> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|_| {
        "unknown relation; use an MCP relation name such as imports or calls".to_owned()
    })
}

fn require_local_transport(hub: Option<&str>, audience: Option<&str>) -> anyhow::Result<()> {
    if hub.is_some() || audience.is_some() {
        anyhow::bail!(
            "hub and OIDC transport are not supported by `know trace`; use a standalone local index"
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct TraceReport {
    workspace: Name,
    registration_issues: Vec<RegistrationIssue>,
    #[serde(flatten)]
    trace: TraceFlowOutput,
}

/// Returns 1 for an unavailable requested start, index or ref, and 2 via the
/// binary's error handler for operational or invalid-input failures.
pub(crate) fn run(args: TraceArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    // Transport refusal never parses a URL or opens configuration/credentials.
    require_local_transport(args.hub.as_deref(), args.oidc_audience.as_deref())?;
    let prepared = Prepared::load(env, args.local.organization)?;
    let input = TraceFlowInput {
        target: Target::workspace(prepared.workspace.name.clone(), Vec::new()),
        symbol: args.symbol,
        id: args.id,
        contract: args.contract,
        project: args.project,
        direction: Some(args.direction.into()),
        max_depth: Some(args.max_depth),
        relations: args.relations,
        limit: Some(args.limit),
        ..TraceFlowInput::default()
    };
    // Direct Engine calls do not pass through the MCP transport's validator.
    input
        .validate()
        .map_err(|error| anyhow::anyhow!(error.client_message("know-trace")))?;
    let projects: Vec<Name> = input.project.clone().into_iter().collect();
    prepared.validate_projects(&projects)?;
    let report = db::runtime()?.block_on(async {
        let local = prepared.open(env).await?;
        let report = async {
            // A project only disambiguates the start. Cross-project edges need
            // every registered visible view, so registration errors stay fatal.
            local.require_registered(&[])?;
            for registered in &local.registration.views {
                local.status(registered).await?;
            }
            let trace = local
                .engine
                .trace_flow(&local_engine::caller(), input)
                .await
                .map_err(|error| anyhow::anyhow!(error.client_message("know-trace")))?;
            Ok::<_, anyhow::Error>(TraceReport {
                workspace: local.workspace.name.clone(),
                registration_issues: local.registration.issues.clone(),
                trace,
            })
        }
        .await;
        local.store().close().await;
        report
    })?;
    let unavailable = requested_source_unavailable(&report.trace);
    let body = render(&report, args.output.format())?;
    args.output.emit(&body, out)?;
    Ok(if unavailable {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn requested_source_unavailable(trace: &TraceFlowOutput) -> bool {
    trace.gaps.iter().any(|gap| {
        matches!(
            gap.reason,
            GapReason::ProjectNotIndexed | GapReason::RefNotFound | GapReason::NotFound
        )
    })
}

fn enum_text(value: &impl Serialize) -> anyhow::Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(text) => Ok(text),
        _ => anyhow::bail!("graph enum did not serialize as a name"),
    }
}

fn render(report: &TraceReport, format: GraphFormat) -> anyhow::Result<String> {
    let markdown = match format {
        GraphFormat::Json => return Ok(serde_json::to_string_pretty(report)?),
        GraphFormat::Text => false,
        GraphFormat::Markdown => true,
    };
    let safe = |value: &str| {
        if markdown {
            graph_output::markdown_text(value)
        } else {
            local_engine::terminal_text(value)
        }
    };
    let mut lines = vec![format!(
        "workspace {}: {} node(s), {} edge(s); truncated: {}",
        safe(report.workspace.as_str()),
        report.trace.nodes.len(),
        report.trace.edges.len(),
        report.trace.truncated,
    )];
    if markdown {
        lines.insert(0, "## Knowell trace".to_owned());
        lines.insert(1, String::new());
    }
    lines.push(String::new());
    lines.push(if markdown { "### Nodes" } else { "nodes:" }.to_owned());
    lines.push(String::new());
    if report.trace.nodes.is_empty() {
        lines.push("no trace nodes were returned".to_owned());
    }
    for node in &report.trace.nodes {
        lines.push(format!(
            "- {} [{}] {}{}",
            safe(&node.node),
            enum_text(&node.kind)?,
            safe(&node.label),
            node.project.as_ref().map_or_else(String::new, |project| {
                format!("; project {}", safe(project.as_str()))
            }),
        ));
        if let Some(id) = &node.id {
            lines.push(format!("  result id: {}", safe(id.as_str())));
        }
        if let Some(evidence) = &node.evidence {
            lines.push(format!(
                "  evidence: {}",
                safe(&graph_output::evidence_text(evidence)),
            ));
        } else {
            lines.push("  no source evidence is attached to this node".to_owned());
        }
    }
    lines.push(String::new());
    lines.push(if markdown { "### Edges" } else { "edges:" }.to_owned());
    lines.push(String::new());
    if report.trace.edges.is_empty() {
        lines.push("no evidenced relation edges were returned".to_owned());
    }
    for edge in &report.trace.edges {
        lines.push(format!(
            "- {} -> {}: {}; evidence {}; resolution {}",
            safe(&edge.from),
            safe(&edge.to),
            enum_text(&edge.relation)?,
            enum_text(&edge.evidence_type)?,
            enum_text(&edge.resolution)?,
        ));
        for evidence in &edge.evidence {
            lines.push(format!(
                "  evidence: {}",
                safe(&graph_output::evidence_text(evidence)),
            ));
        }
    }
    if !report.trace.gaps.is_empty() {
        lines.push(String::new());
        lines.push(if markdown { "### Gaps" } else { "gaps:" }.to_owned());
        lines.push(String::new());
        for gap in &report.trace.gaps {
            lines.push(format!(
                "- [{}]{} {}",
                gap.reason.as_str(),
                gap.project.as_ref().map_or_else(String::new, |project| {
                    format!(" project {}:", safe(project.as_str()))
                }),
                safe(&gap.message),
            ));
        }
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use knowell_mcp::Gap;
    use knowell_mcp::tools::{FlowNode, NodeKind};

    use super::*;

    #[derive(Parser)]
    struct TraceCli {
        #[command(flatten)]
        args: TraceArgs,
    }

    #[test]
    fn start_selection_is_exclusive_but_hub_refusal_can_run_without_a_start() {
        assert!(TraceCli::try_parse_from(["trace"]).is_err());
        for args in [
            ["trace", "--hub", "https://synthetic.invalid"],
            ["trace", "--oidc-audience", "synthetic-audience"],
        ] {
            let parsed = TraceCli::try_parse_from(args).unwrap();
            assert!(parsed.args.symbol.is_none());
            assert!(
                require_local_transport(
                    parsed.args.hub.as_deref(),
                    parsed.args.oidc_audience.as_deref(),
                )
                .is_err()
            );
        }
        assert!(TraceCli::try_parse_from(["trace", "ScopeProbe", "--id", "r1"]).is_err());
        assert!(
            TraceCli::try_parse_from([
                "trace",
                "--contract",
                "topic:synthetic",
                "--project",
                "synthetic",
            ])
            .is_err()
        );
    }

    #[test]
    fn requested_source_failure_is_distinct_from_an_incomplete_trace() {
        for reason in [
            GapReason::ProjectNotIndexed,
            GapReason::RefNotFound,
            GapReason::NotFound,
            GapReason::NoCandidatesInSelectedRef,
            GapReason::RelationsNotReady,
            GapReason::LimitReached,
        ] {
            let trace = TraceFlowOutput {
                gaps: vec![Gap::new(reason, "synthetic gap")],
                ..TraceFlowOutput::default()
            };
            assert_eq!(
                requested_source_unavailable(&trace),
                matches!(
                    reason,
                    GapReason::ProjectNotIndexed | GapReason::RefNotFound | GapReason::NotFound
                ),
            );
        }
    }

    #[test]
    fn hub_transport_is_refused_without_echoing_values() {
        for (hub, audience) in [
            (Some("https://KNOWELL_CANARY_HUB.invalid"), None),
            (None, Some("KNOWELL_CANARY_AUDIENCE")),
        ] {
            let error = require_local_transport(hub, audience).unwrap_err();
            assert!(!error.to_string().contains("KNOWELL_CANARY"));
        }
        assert!(require_local_transport(None, None).is_ok());
    }

    #[test]
    fn relation_names_use_the_mcp_schema_and_reject_hostile_input() {
        assert_eq!(parse_relation("http_call").unwrap(), RelationKind::HttpCall);
        assert_eq!(parse_relation("imports").unwrap(), RelationKind::Imports);
        for value in [
            "",
            "http",
            "http_call\u{0000}",
            "\u{001b}[31m",
            "{\"relation\":",
        ] {
            let error = parse_relation(value).unwrap_err();
            assert!(!error.contains(value) || value.is_empty());
        }
    }

    #[test]
    fn human_trace_output_sanitizes_repository_labels_and_gap_messages() {
        let report = TraceReport {
            workspace: Name::new("synthetic").unwrap(),
            registration_issues: Vec::new(),
            trace: TraceFlowOutput {
                nodes: vec![FlowNode {
                    node: "n0".to_owned(),
                    kind: NodeKind::Symbol,
                    label: "<script>*probe*\n\u{001b}[31m".to_owned(),
                    project: None,
                    id: None,
                    evidence: None,
                }],
                gaps: vec![Gap::new(
                    GapReason::LimitReached,
                    "[probe](https://example.invalid)\u{0007}",
                )],
                ..TraceFlowOutput::default()
            },
        };
        let text = render(&report, GraphFormat::Text).unwrap();
        let markdown = render(&report, GraphFormat::Markdown).unwrap();
        for output in [&text, &markdown] {
            assert!(
                !output
                    .chars()
                    .any(|character| character.is_control() && character != '\n')
            );
            assert!(output.contains("no source evidence"));
            assert!(output.contains("limit_reached"));
        }
        assert!(!markdown.contains("<script>"));
        assert!(!markdown.contains("[probe](https://example.invalid)"));
        let json: serde_json::Value =
            serde_json::from_str(&render(&report, GraphFormat::Json).unwrap()).unwrap();
        assert_eq!(json["nodes"][0]["label"], report.trace.nodes[0].label);
    }
}
