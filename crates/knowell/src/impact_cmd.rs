//! `know impact`: inspect the existing local graph without indexing sources.
//!
//! A project identifies the changed subject; propagation still spans the
//! authorized workspace so cross-project consumers are not silently omitted.

use std::process::ExitCode;

use anyhow::{Context, bail};
use clap::{ArgGroup, Args};
use knowell_core::{Name, RepoPath, TrackTarget};
use knowell_index::RegistrationIssue;
use knowell_mcp::tools::{
    AnalyzeImpactInput, AnalyzeImpactOutput, ChangeSubject, ImpactItem, ImpactKind, RiskCode,
    RiskLevel,
};
use knowell_mcp::{Evidence, GapReason, KnowellTools, ResultId, SymbolRef, Target, Validate};
use serde::Serialize;

use crate::db;
use crate::env::Env;
use crate::graph_output::{self, GraphFormat, GraphOutputArgs};
use crate::local_engine::{self, LocalArgs, Prepared};
use crate::output::Output;

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("impact_subject").args(["symbol", "id", "file", "base"])))]
pub(crate) struct ImpactArgs {
    #[command(flatten)]
    local: LocalArgs,
    #[command(flatten)]
    output: GraphOutputArgs,
    /// Symbol whose dependents to inspect; quote qualified names containing spaces.
    symbol: Option<String>,
    /// Stable result id of the changed symbol.
    #[arg(long, value_name = "RESULT_ID")]
    id: Option<String>,
    /// Changed file, relative to the selected project's source root.
    #[arg(long, value_name = "PATH")]
    file: Option<String>,
    /// Compare from this full commit id or typed ref (e.g. branch:main).
    #[arg(long, alias = "diff-base", value_name = "REF")]
    base: Option<String>,
    /// Compare to this full commit id or typed ref; otherwise use the pinned view.
    #[arg(
        long,
        requires = "base",
        conflicts_with_all = ["symbol", "id", "file"],
        value_name = "REF"
    )]
    head: Option<String>,
    /// Project of the changed file/diff, or defining project of the symbol.
    #[arg(long, value_name = "NAME")]
    project: Option<Name>,
    /// Maximum relation hops (1–5).
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=5))]
    max_depth: u8,
    /// Maximum impacted items and tests per result list (1–200).
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=200))]
    limit: u32,
    /// Omit the suggested test list.
    #[arg(long)]
    no_tests: bool,
    /// Hub transport is not implemented by this local command.
    #[arg(long, alias = "hub-url", value_name = "URL")]
    hub: Option<String>,
    /// Hub authentication is not implemented by this local command.
    #[arg(long, value_name = "AUDIENCE")]
    oidc_audience: Option<String>,
}

#[derive(Debug, Serialize)]
struct ImpactReport {
    workspace: Name,
    registration_issues: Vec<RegistrationIssue>,
    #[serde(flatten)]
    impact: AnalyzeImpactOutput,
}

/// Returns 1 for an absent requested subject/index/ref; input and operational errors propagate.
pub(crate) fn run(args: ImpactArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    // Refuse the action's remote flags before loading configuration, credentials
    // or the database. Their values must never become part of the error.
    require_local_transport(&args)?;
    let change = change_subject(&args)?;
    let diff_subject = matches!(&change, ChangeSubject::Diff { .. });
    let projects: Vec<Name> = args.project.clone().into_iter().collect();
    let prepared = Prepared::load(env, args.local.organization)?;
    prepared.validate_projects(&projects)?;
    let input = AnalyzeImpactInput {
        target: Target::workspace(prepared.workspace.name.clone(), Vec::new()),
        change: Some(change),
        max_depth: Some(args.max_depth),
        include_tests: Some(!args.no_tests),
        limit: Some(args.limit),
        job_id: None,
    };
    // Direct engine calls do not pass through the MCP server's validator.
    input
        .validate()
        .map_err(|error| anyhow::anyhow!(error.client_message("know-impact")))?;
    let report = db::runtime()?.block_on(async {
        let local = prepared.open(env).await?;
        let report = async {
            local.require_registered(&[])?;
            // Every registered view participates in cross-project propagation.
            // A failed status read is an operational error, not a missing index.
            for registered in &local.registration.views {
                local.status(registered).await?;
            }
            let impact = local
                .engine
                .analyze_impact(&local_engine::caller(), input)
                .await
                .map_err(|error| anyhow::anyhow!(error.client_message("know-impact")))?;
            Ok::<_, anyhow::Error>(ImpactReport {
                workspace: local.workspace.name.clone(),
                registration_issues: local.registration.issues.clone(),
                impact,
            })
        }
        .await;
        local.store().close().await;
        report
    })?;
    let format = args.output.format();
    let body = match format {
        GraphFormat::Json => serde_json::to_string_pretty(&report)?,
        GraphFormat::Text | GraphFormat::Markdown => render_report(&report, format)?,
    };
    args.output.emit(&body, out)?;
    out.flush()?;
    Ok(if requested_data_missing(&report.impact, diff_subject) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn require_local_transport(args: &ImpactArgs) -> anyhow::Result<()> {
    if args.hub.is_some() || args.oidc_audience.is_some() {
        bail!("hub transport is not supported by `know impact`; use a local standalone workspace");
    }
    Ok(())
}

fn change_subject(args: &ImpactArgs) -> anyhow::Result<ChangeSubject> {
    let subjects = [
        args.symbol.is_some(),
        args.id.is_some(),
        args.file.is_some(),
        args.base.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if subjects != 1 {
        bail!("pass exactly one symbol, `--id`, `--file` or `--base`");
    }
    if args.head.is_some() && args.base.is_none() {
        bail!("`--head` requires `--base`");
    }
    if let Some(symbol) = &args.symbol {
        let symbol = SymbolRef {
            id: None,
            symbol: Some(symbol.clone()),
            project: args.project.clone(),
        };
        symbol
            .validate()
            .map_err(|error| anyhow::anyhow!(error.client_message("know-impact")))?;
        return Ok(ChangeSubject::Symbol { symbol });
    }
    if let Some(id) = &args.id {
        if args.project.is_some() {
            bail!(
                "`--project` disambiguates a symbol name or selects a file/diff; omit it with `--id`"
            );
        }
        let id =
            ResultId::new(id.clone()).map_err(|_| anyhow::anyhow!("the result id is invalid"))?;
        return Ok(ChangeSubject::Symbol {
            symbol: SymbolRef {
                id: Some(id),
                symbol: None,
                project: None,
            },
        });
    }
    let project = args
        .project
        .clone()
        .context("`--project` is required with `--file` or `--base`")?;
    if let Some(file) = &args.file {
        let path = RepoPath::new(file.clone()).map_err(|_| {
            anyhow::anyhow!(
                "`--file` must be a normalized `/`-separated path relative to the project root"
            )
        })?;
        return Ok(ChangeSubject::File { project, path });
    }
    let base = parse_ref(args.base.as_deref().context("`--base` is required")?)?;
    let head = args.head.as_deref().map(parse_ref).transpose()?;
    if head.as_ref() == Some(&base) {
        bail!("`--base` and `--head` are the same ref");
    }
    Ok(ChangeSubject::Diff {
        project,
        base,
        head,
    })
}

fn parse_ref(value: &str) -> anyhow::Result<TrackTarget> {
    let raw_commit = matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    let target = if raw_commit {
        format!("commit:{value}").parse()
    } else {
        value.parse()
    };
    // TrackTarget errors include the rejected input. Keep this boundary fixed
    // so malformed refs cannot inject terminal controls or reveal a pasted key.
    target.map_err(|_| {
        anyhow::anyhow!("a ref must be a full lowercase commit id, `branch:<name>`, `remote:<remote>/<branch>`, `tag:<name>` or `worktree`")
    })
}

fn requested_data_missing(impact: &AnalyzeImpactOutput, diff_subject: bool) -> bool {
    // A valid comparison can contain no changed indexed items. Keep that
    // distinction from a requested file or symbol that cannot be found.
    impact.gaps.iter().any(|gap| {
        matches!(
            gap.reason,
            GapReason::ProjectNotIndexed | GapReason::RefNotFound
        ) || (!diff_subject && gap.reason == GapReason::NotFound)
    })
}

fn render_report(report: &ImpactReport, format: GraphFormat) -> anyhow::Result<String> {
    let markdown = matches!(format, GraphFormat::Markdown);
    let safe = |value: &str| escaped(value, markdown);
    let mut lines = Vec::new();
    if markdown {
        lines.push("## Knowell impact".to_owned());
        lines.push(String::new());
    }
    lines.push(format!("Workspace: {}", safe(report.workspace.as_str())));
    lines.push(format!("Subject: {}", safe(&report.impact.subject)));
    lines.push(format!(
        "{} changed item(s), {} impacted item(s), {} test(s).",
        report.impact.changed.len(),
        report.impact.impacted.len(),
        report.impact.tests.len(),
    ));
    render_items("Changed", &report.impact.changed, markdown, &mut lines)?;
    render_items("Impacted", &report.impact.impacted, markdown, &mut lines)?;
    render_items("Tests", &report.impact.tests, markdown, &mut lines)?;
    section("Risk", markdown, &mut lines);
    if let Some(risk) = &report.impact.risk {
        lines.push(format!("Level: {}", risk_level(risk.level)));
        if !risk.factors.is_empty() {
            lines.push(String::new());
        }
        for factor in &risk.factors {
            lines.push(format!(
                "- {}: {}",
                risk_code(factor.code),
                safe(&factor.message),
            ));
            for evidence in &factor.evidence {
                render_evidence(evidence, markdown, &mut lines)?;
            }
        }
    } else {
        lines.push("Not assessed; see the reported gaps.".to_owned());
    }
    if !report.impact.gaps.is_empty() {
        section("Gaps", markdown, &mut lines);
        for gap in &report.impact.gaps {
            let project = gap
                .project
                .as_ref()
                .map_or_else(String::new, |project| format!(" ({project})"));
            lines.push(format!(
                "- {}{}: {}",
                gap.reason.as_str(),
                safe(&project),
                safe(&gap.message),
            ));
        }
    }
    for issue in &report.registration_issues {
        lines.push(format!(
            "- Project {} could not be registered: {}",
            safe(issue.project.as_str()),
            safe(&issue.reason),
        ));
    }
    if report.impact.truncated {
        lines.push(String::new());
        lines.push("Result truncated by the depth or item limit.".to_owned());
    }
    if let Some(job) = &report.impact.job {
        lines.push(String::new());
        lines.push(format!(
            "Pending analysis job {}: {}; poll after {} ms.",
            safe(job.job_id.as_str()),
            job.state.as_str(),
            job.poll_after_ms,
        ));
    }
    Ok(lines.join("\n"))
}

fn render_items(
    title: &str,
    items: &[ImpactItem],
    markdown: bool,
    lines: &mut Vec<String>,
) -> anyhow::Result<()> {
    section(title, markdown, lines);
    if items.is_empty() {
        lines.push("None reported.".to_owned());
    }
    for item in items {
        lines.push(format!(
            "- {}: {} ({} hop(s)); id {}",
            impact_kind(item.kind),
            escaped(&item.name, markdown),
            item.distance,
            escaped(item.id.as_str(), markdown),
        ));
        render_evidence(&item.evidence, markdown, lines)?;
    }
    Ok(())
}

fn render_evidence(
    evidence: &Evidence,
    markdown: bool,
    lines: &mut Vec<String>,
) -> anyhow::Result<()> {
    lines.push(format!(
        "  - {}",
        escaped(&graph_output::evidence_text(evidence), markdown),
    ));
    // Keep the typed reasons, including graph hops and resolution, instead of
    // inventing a stronger explanation than the engine's evidence supports.
    for reason in &evidence.why {
        lines.push(format!(
            "  - why: {}",
            escaped(&serde_json::to_string(reason)?, markdown),
        ));
    }
    Ok(())
}

fn section(title: &str, markdown: bool, lines: &mut Vec<String>) {
    lines.push(String::new());
    lines.push(if markdown {
        format!("### {title}")
    } else {
        format!("{title}:")
    });
    lines.push(String::new());
}

fn escaped(value: &str, markdown: bool) -> String {
    if markdown {
        graph_output::markdown_text(value)
    } else {
        local_engine::terminal_text(value)
    }
}

fn impact_kind(kind: ImpactKind) -> &'static str {
    match kind {
        ImpactKind::Symbol => "symbol",
        ImpactKind::File => "file",
        ImpactKind::Contract => "contract",
        ImpactKind::Test => "test",
        ImpactKind::Config => "config",
    }
}

fn risk_level(level: RiskLevel) -> &'static str {
    match level {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
    }
}

fn risk_code(code: RiskCode) -> &'static str {
    match code {
        RiskCode::PublicContractChanged => "public_contract_changed",
        RiskCode::CrossProjectConsumers => "cross_project_consumers",
        RiskCode::BreakingSignatureChange => "breaking_signature_change",
        RiskCode::UntestedCode => "untested_code",
        RiskCode::UnresolvedReferences => "unresolved_references",
        RiskCode::MigrationRequired => "migration_required",
        RiskCode::ManyDependents => "many_dependents",
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use knowell_core::{ContentHash, LineRange};
    use knowell_mcp::{
        CommitId, EvidenceType, FreshnessTier, Gap, GraphHop, IndexState, MatchReason,
        RelationKind, Resolution, ViewLayer,
    };

    use super::*;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        impact: ImpactArgs,
    }

    fn args(values: &[&str]) -> ImpactArgs {
        TestCli::try_parse_from(values).unwrap().impact
    }

    #[test]
    fn committed_diff_accepts_raw_shas_and_typed_refs_without_guessing_branches() {
        let sha = "a".repeat(40);
        let input = args(&[
            "impact",
            "--diff-base",
            &sha,
            "--head",
            "branch:main",
            "--project",
            "alpha",
        ]);
        let ChangeSubject::Diff {
            project,
            base,
            head,
        } = change_subject(&input).unwrap()
        else {
            panic!("expected a committed diff subject");
        };
        assert_eq!(project.as_str(), "alpha");
        assert_eq!(base.to_string(), format!("commit:{sha}"));
        assert_eq!(head.unwrap().to_string(), "branch:main");
        for invalid in [
            "main",
            "abcdef0",
            "branch:",
            "tag:bad..ref",
            "branch:\u{1b}fake",
        ] {
            assert!(parse_ref(invalid).is_err());
        }
        let rejected = "KNOWELL_CANARY_REJECTED_REF";
        assert!(
            !parse_ref(rejected)
                .unwrap_err()
                .to_string()
                .contains(rejected)
        );
        assert_eq!(
            parse_ref(&"b".repeat(64)).unwrap().to_string(),
            format!("commit:{}", "b".repeat(64))
        );
    }

    #[test]
    fn identical_typed_refs_follow_the_mcp_validation_contract() {
        let args = args(&[
            "impact",
            "--base",
            "branch:main",
            "--head",
            "branch:main",
            "--project",
            "alpha",
        ]);
        assert!(change_subject(&args).is_err());
        let input = AnalyzeImpactInput {
            target: Target::workspace(Name::new("synthetic").unwrap(), Vec::new()),
            change: Some(ChangeSubject::Diff {
                project: Name::new("alpha").unwrap(),
                base: "branch:main".parse().unwrap(),
                head: Some("branch:main".parse().unwrap()),
            }),
            ..AnalyzeImpactInput::default()
        };
        assert!(input.validate().is_err());
    }

    #[test]
    fn file_and_symbol_subjects_enforce_project_and_typed_input_constraints() {
        assert!(change_subject(&args(&["impact"])).is_err());
        assert!(change_subject(&args(&["impact", "--file", "src/lib.rs"])).is_err());
        let selected = args(&["impact", "--file", "src/lib.rs", "--project", "alpha"]);
        assert!(matches!(
            change_subject(&selected).unwrap(),
            ChangeSubject::File { .. }
        ));
        let bad = args(&[
            "impact",
            "--file",
            "../KNOWELL_CANARY_PATH",
            "--project",
            "alpha",
        ]);
        let error = change_subject(&bad).unwrap_err().to_string();
        assert!(!error.contains("KNOWELL_CANARY_PATH"));
        assert!(change_subject(&args(&["impact", "--id", "bad id"])).is_err());
        assert!(
            change_subject(&args(&["impact", "--id", "result_1", "--project", "alpha"])).is_err()
        );
        assert!(change_subject(&args(&["impact", " "])).is_err());
        assert!(change_subject(&args(&["impact", &"a".repeat(513)])).is_err());
        assert!(TestCli::try_parse_from(["impact", "symbol", "--file", "src/lib.rs"]).is_err());
        assert!(TestCli::try_parse_from(["impact", "symbol", "--head", "branch:main"]).is_err());
        assert!(
            TestCli::try_parse_from(["impact", "--id", "result_1", "--head", "branch:main"])
                .is_err()
        );
        assert!(
            TestCli::try_parse_from([
                "impact",
                "--file",
                "src/lib.rs",
                "--project",
                "alpha",
                "--head",
                "branch:main"
            ])
            .is_err()
        );
    }

    #[test]
    fn hostile_result_id_errors_discard_the_rejected_input() {
        for value in [
            "KNOWELL_CANARY_ID\u{1b}[31m",
            "KNOWELL_CANARY_ID with whitespace",
            &"KNOWELL_CANARY_ID".repeat(40),
        ] {
            let error = change_subject(&args(&["impact", "--id", value])).unwrap_err();
            let rendered = format!("{error:#}");
            assert_eq!(rendered, "the result id is invalid");
            assert!(!rendered.contains("KNOWELL_CANARY_ID"));
            assert!(!rendered.chars().any(char::is_control));
        }
    }

    #[test]
    fn unsupported_hub_is_explicit_without_a_subject_and_never_echoes_values() {
        let input = args(&[
            "impact",
            "--hub",
            "https://KNOWELL_CANARY_HUB.invalid",
            "--oidc-audience",
            "KNOWELL_CANARY_AUDIENCE",
        ]);
        let error = require_local_transport(&input).unwrap_err().to_string();
        assert!(error.contains("hub transport is not supported"));
        assert!(!error.contains("KNOWELL_CANARY"));
    }

    #[test]
    fn markdown_keeps_gaps_and_absent_risk_without_active_repository_markup() {
        let report = ImpactReport {
            workspace: Name::new("alpha").unwrap(),
            registration_issues: Vec::new(),
            impact: AnalyzeImpactOutput {
                subject: "<script>\u{1b}[31m [subject](https://example.invalid)".into(),
                gaps: vec![Gap::new(GapReason::RefNotFound, "<img src=x>\n# injected")],
                truncated: true,
                ..AnalyzeImpactOutput::default()
            },
        };
        let rendered = render_report(&report, GraphFormat::Markdown).unwrap();
        assert!(rendered.contains("\\<script\\>"));
        assert!(!rendered.contains("<script>"));
        assert!(!rendered.contains("<img src=x>"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains("\n# injected"));
        assert!(!rendered.contains("[subject](https://example.invalid)"));
        assert!(rendered.contains("ref_not_found"));
        assert!(rendered.contains("Not assessed"));
        assert!(rendered.contains("Result truncated"));
    }

    #[test]
    fn human_impact_includes_typed_graph_path_explanations_and_sanitizes_their_labels() {
        let evidence = Evidence {
            project: Name::new("alpha").unwrap(),
            view: "branch:main".parse().unwrap(),
            layer: ViewLayer::Shared,
            commit: CommitId::new("a".repeat(40)).unwrap(),
            path: RepoPath::new("src/probe.ts").unwrap(),
            lines: LineRange::new(1, 3).unwrap(),
            content_hash: ContentHash::of(b"synthetic source"),
            symbol: Some("ScopeProbe".into()),
            why: vec![MatchReason::GraphPath {
                hops: vec![GraphHop {
                    from: "[caller](https://example.invalid)\u{1b}".into(),
                    relation: RelationKind::Imports,
                    to: "<script>ScopeProbe".into(),
                    evidence_type: EvidenceType::SyntacticObservation,
                    resolution: Resolution::Resolved,
                }],
            }],
            freshness: FreshnessTier::T3Relations,
            index_state: IndexState::Stale,
        };
        let report = ImpactReport {
            workspace: Name::new("synthetic").unwrap(),
            registration_issues: Vec::new(),
            impact: AnalyzeImpactOutput {
                subject: "symbol ScopeProbe".into(),
                impacted: vec![ImpactItem {
                    id: ResultId::new("synthetic_result").unwrap(),
                    kind: ImpactKind::File,
                    name: "src/probe.ts".into(),
                    distance: 1,
                    evidence,
                }],
                ..AnalyzeImpactOutput::default()
            },
        };
        let text = render_report(&report, GraphFormat::Text).unwrap();
        assert!(text.contains("why: {\"kind\":\"graph_path\""));
        assert!(text.contains("\"relation\":\"imports\""));
        assert!(text.contains("\"evidence_type\":\"syntactic_observation\""));
        assert!(text.contains("\"resolution\":\"resolved\""));
        assert!(text.contains(&"a".repeat(40)));
        assert!(text.contains("index stale"));
        let markdown = render_report(&report, GraphFormat::Markdown).unwrap();
        assert!(markdown.contains("why:"));
        assert!(markdown.contains("graph\\_path"));
        assert!(!markdown.contains("[caller](https://example.invalid)"));
        assert!(!markdown.contains("<script>"));
        assert!(
            !markdown
                .chars()
                .any(|character| character.is_control() && character != '\n')
        );
    }

    #[test]
    fn only_missing_requested_data_gaps_make_the_analysis_unsuccessful() {
        for reason in [
            GapReason::NotFound,
            GapReason::ProjectNotIndexed,
            GapReason::RefNotFound,
            GapReason::NoCandidatesInSelectedRef,
            GapReason::NoReferenceResolutionForLanguage,
            GapReason::RelationsNotReady,
            GapReason::ExcludedByPolicy,
        ] {
            let impact = AnalyzeImpactOutput {
                gaps: vec![Gap::new(reason, "synthetic gap")],
                ..AnalyzeImpactOutput::default()
            };
            assert_eq!(
                requested_data_missing(&impact, false),
                matches!(
                    reason,
                    GapReason::NotFound | GapReason::ProjectNotIndexed | GapReason::RefNotFound
                )
            );
            assert_eq!(
                requested_data_missing(&impact, true),
                matches!(
                    reason,
                    GapReason::ProjectNotIndexed | GapReason::RefNotFound
                )
            );
        }
    }
}
