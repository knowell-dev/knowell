//! Read-only local catalogue of persisted embedding profile metadata.

use std::process::ExitCode;

use clap::{ArgGroup, Args, Subcommand};
use knowell_core::Name;
use knowell_engine::{ProfileMetadata, ProfileSelector};
use serde::Serialize;

use crate::db;
use crate::env::Env;
use crate::graph_output::{self, GraphFormat, GraphOutputArgs};
use crate::local_engine::{self, CataloguePrepared};
use crate::output::Output;

#[derive(Debug, Subcommand)]
pub(crate) enum ProfileCommand {
    /// List profiles already persisted in the selected organization.
    List(ProfileListArgs),
    /// Show one persisted profile by its name or UUID.
    Show(ProfileShowArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ProfileListArgs {
    #[command(flatten)]
    local: CatalogueArgs,
    #[command(flatten)]
    output: GraphOutputArgs,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("profile_selector").args(["name", "id"]).required(true).multiple(false)))]
pub(crate) struct ProfileShowArgs {
    #[command(flatten)]
    local: CatalogueArgs,
    #[command(flatten)]
    output: GraphOutputArgs,
    /// Profile name, unique within the selected organization.
    #[arg(value_name = "NAME")]
    name: Option<String>,
    /// Persisted profile UUID instead of a name.
    #[arg(long, value_name = "UUID")]
    id: Option<String>,
}

#[derive(Debug, Args)]
struct CatalogueArgs {
    /// Organization whose persisted profile catalogue to read.
    #[arg(long, default_value = "local", value_name = "NAME")]
    organization: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ProfileReport {
    List {
        organization: Name,
        profiles: Vec<ProfileMetadata>,
    },
    Show {
        organization: Name,
        profile: Option<ProfileMetadata>,
    },
}

/// Reads the engine's persisted catalogue without selecting or registering sources.
pub(crate) fn run(
    command: ProfileCommand,
    env: &Env,
    out: &mut Output,
) -> anyhow::Result<ExitCode> {
    let (local_args, output, selector) = match command {
        ProfileCommand::List(args) => (args.local, args.output, None),
        ProfileCommand::Show(args) => (
            args.local,
            args.output,
            Some(parse_selector(args.name.as_deref(), args.id.as_deref())?),
        ),
    };
    let organization = parse_organization(&local_args.organization)?;
    let prepared = CataloguePrepared::load(env, organization)?;
    let report = db::runtime()?.block_on(async {
        let local = prepared.open(env).await?;
        let result = async {
            let caller = local_engine::caller();
            match selector {
                Some(selector) => {
                    let profile = local
                        .engine
                        .get_embedding_profile(&caller, selector)
                        .await
                        .map_err(|error| anyhow::anyhow!(error.client_message("know-profile")))?;
                    Ok::<_, anyhow::Error>(ProfileReport::Show {
                        organization: local.organization.clone(),
                        profile,
                    })
                }
                None => {
                    let profiles = local
                        .engine
                        .list_embedding_profiles(&caller)
                        .await
                        .map_err(|error| anyhow::anyhow!(error.client_message("know-profile")))?;
                    Ok(ProfileReport::List {
                        organization: local.organization.clone(),
                        profiles,
                    })
                }
            }
        }
        .await;
        local.engine.store().close().await;
        result
    })?;
    let body = match output.format() {
        GraphFormat::Json => serde_json::to_string_pretty(&report)?,
        format => render_report(&report, format),
    };
    output.emit(&body, out)?;
    Ok(
        if matches!(report, ProfileReport::Show { profile: None, .. }) {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        },
    )
}

fn parse_organization(value: &str) -> anyhow::Result<Name> {
    Name::new(value).map_err(|_| anyhow::anyhow!("the organization name is invalid"))
}

fn parse_selector(name: Option<&str>, id: Option<&str>) -> anyhow::Result<ProfileSelector> {
    match (name, id) {
        (Some(name), None) => Name::new(name)
            .map(ProfileSelector::Name)
            .map_err(|_| anyhow::anyhow!("the profile name is invalid")),
        (None, Some(id)) => uuid::Uuid::parse_str(id)
            .map(ProfileSelector::Id)
            .map_err(|_| anyhow::anyhow!("the profile id must be a UUID")),
        _ => anyhow::bail!("select one profile name or --id UUID"),
    }
}

fn safe(value: &str, markdown: bool) -> String {
    if markdown {
        graph_output::markdown_text(value)
    } else {
        local_engine::terminal_text(value)
    }
}

fn render_report(report: &ProfileReport, format: GraphFormat) -> String {
    let markdown = matches!(format, GraphFormat::Markdown);
    let mut lines = Vec::new();
    if markdown {
        lines.push("## Knowell embedding profiles".to_owned());
        lines.push(String::new());
    }
    let organization = match report {
        ProfileReport::List { organization, .. } | ProfileReport::Show { organization, .. } => {
            organization
        }
    };
    lines.push(format!(
        "Organization: {}",
        safe(organization.as_str(), markdown)
    ));
    match report {
        ProfileReport::List { profiles, .. } => {
            lines.push(format!("{} profile(s).", profiles.len()));
            for profile in profiles {
                render_profile(profile, markdown, &mut lines);
            }
        }
        ProfileReport::Show {
            profile: Some(profile),
            ..
        } => render_profile(profile, markdown, &mut lines),
        ProfileReport::Show { profile: None, .. } => {
            lines.push("Profile not found in this organization.".to_owned());
        }
    }
    lines.join("\n")
}

fn render_profile(profile: &ProfileMetadata, markdown: bool, lines: &mut Vec<String>) {
    lines.push(String::new());
    let name = safe(profile.name.as_str(), markdown);
    lines.push(if markdown {
        format!("### Profile {name}")
    } else {
        format!("Profile {name}")
    });
    lines.push(format!("Id: {}", profile.id));
    lines.push(format!("Provider: {}", safe(&profile.provider, markdown)));
    lines.push(format!("Model: {}", safe(&profile.model, markdown)));
    lines.push(format!("Dimensions: {}", profile.dimensions));
    lines.push(format!(
        "Input format version: {}",
        safe(&profile.input_format_version, markdown)
    ));
    lines.push(format!(
        "Created (UTC): {}",
        safe(profile.created_at.as_str(), markdown)
    ));
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct ProfileCli {
        #[command(subcommand)]
        command: ProfileCommand,
    }

    #[test]
    fn show_requires_one_name_or_id_and_output_options_are_exclusive() {
        let id = "00000000-0000-0000-0000-000000000001";
        assert!(ProfileCli::try_parse_from(["know", "show", "synthetic"]).is_ok());
        assert!(ProfileCli::try_parse_from(["know", "show", "--id", id]).is_ok());
        assert!(ProfileCli::try_parse_from(["know", "show"]).is_err());
        assert!(ProfileCli::try_parse_from(["know", "show", "synthetic", "--id", id]).is_err());
        assert!(
            ProfileCli::try_parse_from(["know", "list", "--format", "markdown", "--json"]).is_err()
        );
    }

    #[test]
    fn malformed_profile_selectors_and_organization_do_not_echo_rejected_text() {
        let values = [
            String::new(),
            "KNOWELL_CANARY_SELECTOR\u{001b}\n".to_owned(),
            "x".repeat(300),
            "../synthetic".to_owned(),
        ];
        for value in &values {
            for error in [
                parse_selector(Some(value), None).unwrap_err(),
                parse_selector(None, Some(value)).unwrap_err(),
                parse_organization(value).unwrap_err(),
            ] {
                let message = error.to_string();
                assert!(!message.contains(value) || value.is_empty());
                assert!(!message.contains("KNOWELL_CANARY"));
                assert!(!message.chars().any(char::is_control));
            }
        }
        assert!(parse_selector(None, None).is_err());
        assert!(parse_selector(Some("synthetic"), Some("invalid")).is_err());
    }

    #[test]
    fn uuid_shaped_profile_names_are_not_inferred_as_ids() {
        let value = "00000000-0000-0000-0000-000000000001";
        assert_eq!(
            parse_selector(Some(value), None).unwrap(),
            ProfileSelector::Name(Name::new(value).unwrap())
        );
        assert_eq!(
            parse_selector(None, Some(value)).unwrap(),
            ProfileSelector::Id(uuid::Uuid::from_u128(1))
        );
    }

    #[test]
    fn profile_labels_are_exact_in_json_and_safe_in_human_output() {
        let label = "Synthetic ![model](https://example.invalid) <script> ```\u{001b}[31m\t";
        let profile = ProfileMetadata {
            id: uuid::Uuid::from_u128(1),
            name: Name::new("synthetic").unwrap(),
            provider: label.to_owned(),
            model: label.to_owned(),
            dimensions: 768,
            input_format_version: label.to_owned(),
            created_at: knowell_mcp::Timestamp::new("2026-01-02T03:04:05.123456Z").unwrap(),
        };
        let report = ProfileReport::Show {
            organization: Name::new("local").unwrap(),
            profile: Some(profile),
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["profile"]["model"], label);
        let text = render_report(&report, GraphFormat::Text);
        assert!(!text.contains(['\u{001b}', '\t']));
        assert!(text.contains("Created (UTC): 2026-01-02T03:04:05.123456Z"));
        let markdown = render_report(&report, GraphFormat::Markdown);
        assert!(markdown.contains("\\!\\[model\\]\\(https://example.invalid\\)"));
        assert!(markdown.contains("\\<script\\>"));
        assert!(markdown.contains("\\`\\`\\`"));
        assert!(!markdown.contains(['\u{001b}', '\t']));
        for field in [
            "active",
            "local_only",
            "budget",
            "estimate",
            "switch",
            "rollback",
        ] {
            assert!(json["profile"].get(field).is_none());
        }
    }
}
