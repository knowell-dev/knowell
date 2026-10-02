//! `know config …`

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::{Subcommand, ValueEnum};
use knowell_config::{ResolvedWorkspace, Sourced};

use crate::output::Output;

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Validate a workspace file (knowell.toml) and show every effective
    /// setting together with where it came from.
    Check {
        /// Workspace configuration file.
        #[arg(default_value = "knowell.toml")]
        workspace: PathBuf,
        /// Engine configuration to check the workspace against (providers,
        /// data policy compatibility).
        #[arg(long)]
        engine: Option<PathBuf>,
        /// Print the resolved workspace as JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Print the JSON Schema of a configuration file (for editor completion).
    Schema {
        /// Which file.
        kind: SchemaKind,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum SchemaKind {
    /// The engine file (`~/.knowell/config.toml`).
    Engine,
    /// A workspace file (`knowell.toml`).
    Workspace,
}

pub(crate) fn run(cmd: ConfigCommand, out: &mut Output) -> anyhow::Result<ExitCode> {
    match cmd {
        ConfigCommand::Check {
            workspace,
            engine,
            json,
        } => check(&workspace, engine.as_deref(), json, out),
        ConfigCommand::Schema { kind } => {
            let schema = match kind {
                SchemaKind::Engine => knowell_config::engine_schema(),
                SchemaKind::Workspace => knowell_config::workspace_schema(),
            };
            out.line(serde_json::to_string_pretty(&schema)?)?;
            out.flush()?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn check(
    workspace_path: &Path,
    engine_path: Option<&Path>,
    json: bool,
    out: &mut Output,
) -> anyhow::Result<ExitCode> {
    // ConfigError's Display never contains configuration values, so it is
    // safe to show even when a secret was pasted into the file.
    let config = match knowell_config::load_workspace(workspace_path) {
        Ok(config) => config,
        Err(err) => {
            tracing::error!("{err}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let base_dir = workspace_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let base_dir = std::path::absolute(base_dir)
        .with_context(|| format!("cannot resolve directory {}", base_dir.display()))?;
    let resolved = match config.resolve(&base_dir) {
        Ok(resolved) => resolved,
        Err(issues) => {
            tracing::error!("{}: {issues}", workspace_path.display());
            return Ok(ExitCode::FAILURE);
        }
    };

    let mut issues = Vec::new();
    if let Some(engine_path) = engine_path {
        match knowell_config::load_engine(engine_path) {
            Ok(engine) => issues = resolved.check_against(&engine),
            Err(err) => {
                tracing::error!("{err}");
                return Ok(ExitCode::FAILURE);
            }
        }
    }

    if json {
        out.line(serde_json::to_string_pretty(&resolved)?)?;
    } else {
        print_resolved(&resolved, out)?;
    }
    for issue in &issues {
        tracing::error!("{issue}");
    }
    out.flush()?;
    Ok(if issues.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn print_resolved(ws: &ResolvedWorkspace, out: &mut Output) -> anyhow::Result<()> {
    out.line(format!(
        "workspace {} — {} project(s)",
        ws.name,
        ws.projects.len()
    ))?;
    for project in &ws.projects {
        out.line("")?;
        out.line(format!("project {}", project.name))?;
        out.line(format!("  path         {}", project.path.display()))?;
        if let Some(root) = &project.root {
            out.line(format!("  root         {root}"))?;
        }
        out.line(format!("  track        {}", sourced(&project.track)))?;
        out.line(format!(
            "  data policy  {} ({})",
            project.data_policy.value.as_str(),
            project.data_policy.origin
        ))?;
        let emb = &project.embedding;
        let provider = emb
            .provider
            .as_ref()
            .map_or_else(|| "none (lexical only)".to_owned(), sourced);
        out.line(format!("  embedding    provider {provider}"))?;
        out.line(format!(
            "               preset {} ({}), {} dimensions ({})",
            emb.preset.value.as_str(),
            emb.preset.origin,
            emb.dimensions.value,
            emb.dimensions.origin
        ))?;
        if let Some(model) = &emb.model {
            out.line(format!("               model {}", sourced(model)))?;
        }
        if project.exclude.is_empty() {
            out.line("  exclude      (built-in sensitive-file rules only)")?;
        } else {
            for (i, pattern) in project.exclude.iter().enumerate() {
                let label = if i == 0 {
                    "  exclude    "
                } else {
                    "             "
                };
                out.line(format!("{label}  {}", sourced(pattern)))?;
            }
        }
    }
    Ok(())
}

fn sourced<T: std::fmt::Display>(s: &Sourced<T>) -> String {
    format!("{} ({})", s.value, s.origin)
}
