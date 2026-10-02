//! `know ci init`: CI integration files from `knowell-setup` templates.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Subcommand, ValueEnum};
use knowell_setup::{CiMode, CiOptions, CiProvider, SetupError};

use crate::fsutil;
use crate::output::Output;

#[derive(Debug, Subcommand)]
pub(crate) enum CiCommand {
    /// Write the pipeline file for a CI system into a repository.
    Init {
        /// CI system.
        #[arg(value_enum)]
        provider: ProviderArg,
        /// What the pipeline does.
        #[arg(long, value_enum, default_value_t = ModeArg::Check)]
        mode: ModeArg,
        /// `https://` URL of the team's hub (check-and-impact, index-update).
        #[arg(long, value_name = "URL")]
        hub_url: Option<String>,
        /// Branch whose pushes update the index (index-update).
        #[arg(long, value_name = "BRANCH")]
        branch: Option<String>,
        /// Repository root to write into.
        #[arg(long, value_name = "DIR", default_value = ".")]
        dir: PathBuf,
        /// Print the files instead of writing them.
        #[arg(long)]
        dry_run: bool,
        /// Replace files that exist with different content.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ProviderArg {
    /// GitHub Actions.
    Github,
    /// GitLab CI/CD.
    Gitlab,
    /// Gitea Actions.
    Gitea,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ModeArg {
    /// `know check` on pull requests; no hub, no credentials, fork-safe.
    Check,
    /// Check plus cross-project impact through the hub (same-repository PRs).
    CheckAndImpact,
    /// Check plus an index update on pushes to --branch.
    IndexUpdate,
}

pub(crate) fn run(cmd: CiCommand, out: &mut Output) -> anyhow::Result<ExitCode> {
    let CiCommand::Init {
        provider,
        mode,
        hub_url,
        branch,
        dir,
        dry_run,
        force,
    } = cmd;
    let provider = match provider {
        ProviderArg::Github => CiProvider::GitHub,
        ProviderArg::Gitlab => CiProvider::GitLab,
        ProviderArg::Gitea => CiProvider::Gitea,
    };
    let options = CiOptions {
        mode: match mode {
            ModeArg::Check => CiMode::Check,
            ModeArg::CheckAndImpact => CiMode::CheckAndImpact,
            ModeArg::IndexUpdate => CiMode::IndexUpdate,
        },
        hub_url,
        tracked_branch: branch,
    };
    let files = match knowell_setup::ci_init(provider, &options) {
        Ok(files) => files,
        Err(err @ SetupError::InvalidInput(_)) => {
            tracing::error!("{err}");
            return Ok(ExitCode::from(2));
        }
        Err(err) => return Err(err.into()),
    };

    // Decide for every file before writing any, so a refusal changes nothing.
    let mut plan = Vec::new();
    let mut refused = Vec::new();
    for file in &files {
        let target = dir.join(&file.path);
        let state = match fsutil::read_optional(&target)? {
            None => FileState::Create,
            Some(existing) if existing == file.content => FileState::Unchanged,
            Some(_) if force => FileState::Replace,
            Some(_) => {
                refused.push(target);
                continue;
            }
        };
        plan.push((file, target, state));
    }
    if !refused.is_empty() {
        for path in &refused {
            tracing::error!(
                "{} exists with different content; pass --force to replace it (or --dry-run to compare)",
                path.display()
            );
        }
        return Ok(ExitCode::FAILURE);
    }
    for (file, target, state) in &plan {
        if dry_run {
            out.line(format!(
                "--- {} ({})",
                fsutil::slash_path(&file.path),
                state.label()
            ))?;
            out.line(file.content.trim_end())?;
        } else {
            if *state != FileState::Unchanged {
                fsutil::write_atomic(target, &file.content)?;
            }
            out.line(format!("{} {}", state.done(), target.display()))?;
        }
        if let Some(note) = &file.note {
            out.line(format!("note: {note}"))?;
        }
    }
    if dry_run {
        out.line("dry run: nothing was written")?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileState {
    Create,
    Replace,
    Unchanged,
}

impl FileState {
    fn label(self) -> &'static str {
        match self {
            FileState::Create => "new file",
            FileState::Replace => "replaces existing file",
            FileState::Unchanged => "unchanged",
        }
    }

    fn done(self) -> &'static str {
        match self {
            FileState::Create => "created",
            FileState::Replace => "replaced",
            FileState::Unchanged => "unchanged",
        }
    }
}
