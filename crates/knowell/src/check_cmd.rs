//! `know check`: deterministic contract and rule checks, no API keys needed.
//!
//! Reads every project of the workspace through the secret boundary,
//! extracts contracts with the built-in rule packs, links producers and
//! consumers across projects and reports findings as text, JSON or SARIF.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, ValueEnum};
use knowell_core::{Name, RepoPath, TrackTarget};
use knowell_link::{
    CheckOptions, ExtractOptions, Finding, LinkOptions, PackSet, ProjectExtractions, Severity,
};
use knowell_secrets::ExclusionPolicy;
use knowell_source::WalkOptions;
use knowell_source::git::{Change, GitRepo};

use crate::env::Env;
use crate::output::Output;

#[derive(Debug, Args)]
pub(crate) struct CheckArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
    /// Write the report to this file instead of stdout.
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,
    /// Lowest severity that makes the command exit with status 1.
    #[arg(long, value_enum, default_value_t = FailOn::Error)]
    fail_on: FailOn,
    /// Only report findings located in files changed since this commit
    /// (full commit id, e.g. the pull request base).
    #[arg(long, value_name = "SHA")]
    diff_base: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    /// Human-readable list.
    Text,
    /// Findings as JSON.
    Json,
    /// SARIF 2.1.0 (GitHub code scanning).
    Sarif,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum FailOn {
    /// Fail on errors only.
    Error,
    /// Fail on warnings and errors.
    Warning,
    /// Never fail because of findings.
    Never,
}

impl FailOn {
    fn fails(self, severity: Severity) -> bool {
        match self {
            FailOn::Never => false,
            FailOn::Error => severity == Severity::Error,
            FailOn::Warning => matches!(severity, Severity::Warning | Severity::Error),
        }
    }
}

/// One project to analyse: name, directory on disk and exclusion policy.
struct ProjectInput {
    name: Name,
    dir: PathBuf,
    repo_dir: PathBuf,
    root: Option<RepoPath>,
    policy: ExclusionPolicy,
}

pub(crate) fn run(args: CheckArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let workspace_path = env.require_workspace()?;
    let config = match knowell_config::load_workspace(&workspace_path) {
        Ok(config) => config,
        Err(err) => {
            tracing::error!("{err}");
            return Ok(ExitCode::from(2));
        }
    };
    let base_dir = crate::env::parent_dir(&workspace_path)?;
    let resolved = match config.resolve(&base_dir) {
        Ok(resolved) => resolved,
        Err(issues) => {
            tracing::error!("{}: {issues}", workspace_path.display());
            return Ok(ExitCode::from(2));
        }
    };

    let mut inputs = Vec::new();
    for project in &resolved.projects {
        let patterns: Vec<String> = project.exclude.iter().map(|s| s.value.clone()).collect();
        let policy = ExclusionPolicy::with_patterns(&patterns)
            .with_context(|| format!("invalid exclude pattern in project `{}`", project.name))?;
        let dir = match &project.root {
            Some(root) => project.path.join(root.as_str()),
            None => project.path.clone(),
        };
        inputs.push(ProjectInput {
            name: project.name.clone(),
            dir,
            repo_dir: project.path.clone(),
            root: project.root.clone(),
            policy,
        });
    }

    let packs = PackSet::builtin().context("built-in rule packs failed to load")?;
    let mut extractions: Vec<ProjectExtractions> = Vec::with_capacity(inputs.len());
    for input in &inputs {
        extractions.push(extract(input, &packs)?);
    }
    let linked = knowell_link::link(&extractions, &LinkOptions::default())?;
    let mut findings = knowell_link::check(&extractions, &linked, &CheckOptions::default())?;

    if let Some(base) = &args.diff_base {
        match changed_files(&inputs, base) {
            Some(changed) => findings.retain(|f| touches(f, &changed)),
            None => tracing::warn!(
                "--diff-base {base} was not found in any project repository; reporting all findings"
            ),
        }
    }

    let failing = findings
        .iter()
        .filter(|f| args.fail_on.fails(f.severity))
        .count();
    let report = render(&findings, args.format, &inputs)?;
    match &args.output {
        Some(path) => std::fs::write(path, report)
            .with_context(|| format!("cannot write {}", path.display()))?,
        None => out.line(report.trim_end())?,
    }
    out.flush()?;
    tracing::info!(
        "{} finding(s) across {} project(s); {} at or above --fail-on",
        findings.len(),
        inputs.len(),
        failing
    );
    Ok(if failing > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Reads one project through the secret boundary and runs the rule packs.
fn extract(input: &ProjectInput, packs: &PackSet) -> anyhow::Result<ProjectExtractions> {
    let report = knowell_source::walk(&input.dir, &input.policy, &WalkOptions::default())
        .with_context(|| format!("cannot read project `{}`", input.name))?;
    let mut texts: BTreeMap<RepoPath, String> = BTreeMap::new();
    for file in report.files {
        texts.insert(file.path, file.text);
    }
    let paths: Vec<RepoPath> = texts.keys().cloned().collect();
    let options = ExtractOptions {
        exclusion: input.policy.clone(),
        ..ExtractOptions::default()
    };
    Ok(knowell_link::extract_project(
        &input.name,
        &paths,
        packs,
        &options,
        &mut |path| texts.get(path).cloned(),
    ))
}

/// Files changed between `base` and `HEAD`, as (project, project-relative
/// path), for every project whose repository contains `base`. `None` when no
/// repository knows the commit.
fn changed_files(inputs: &[ProjectInput], base: &str) -> Option<BTreeSet<(Name, RepoPath)>> {
    let base_target: TrackTarget = format!("commit:{base}").parse().ok()?;
    let mut changed = BTreeSet::new();
    let mut found = false;
    for input in inputs {
        let Ok(repo) = GitRepo::open(&input.repo_dir) else {
            continue;
        };
        let (Ok(old), Ok(new)) = (
            repo.resolve(&base_target),
            repo.resolve(&TrackTarget::WorktreeHead),
        ) else {
            continue;
        };
        let Ok(changes) = repo.diff(&old.commit, &new.commit) else {
            continue;
        };
        found = true;
        for change in changes {
            for path in change_paths(&change) {
                if let Some(rel) = relative_to_root(path, input.root.as_ref()) {
                    changed.insert((input.name.clone(), rel));
                }
            }
        }
    }
    found.then_some(changed)
}

fn change_paths(change: &Change) -> Vec<&RepoPath> {
    match change {
        Change::Added(p) | Change::Modified(p) | Change::Deleted(p) => vec![p],
        Change::Renamed { from, to, .. } => vec![from, to],
    }
}

/// Maps a repository-relative path into a project with a sub-root.
fn relative_to_root(path: &RepoPath, root: Option<&RepoPath>) -> Option<RepoPath> {
    match root {
        None => Some(path.clone()),
        Some(root) => {
            let rest = path
                .as_str()
                .strip_prefix(root.as_str())?
                .strip_prefix('/')?;
            RepoPath::new(rest).ok()
        }
    }
}

fn touches(finding: &Finding, changed: &BTreeSet<(Name, RepoPath)>) -> bool {
    finding
        .locations
        .iter()
        .any(|l| changed.contains(&(l.project.clone(), l.path.clone())))
}

fn render(findings: &[Finding], format: Format, inputs: &[ProjectInput]) -> anyhow::Result<String> {
    Ok(match format {
        Format::Sarif => {
            let roots = inputs
                .iter()
                .map(|input| {
                    let path = std::fs::canonicalize(&input.dir).with_context(|| {
                        format!("cannot resolve SARIF root for project `{}`", input.name)
                    })?;
                    let uri = url::Url::from_directory_path(path).map_err(|_| {
                        anyhow::anyhow!(
                            "cannot represent SARIF root for project `{}` as a file URI",
                            input.name
                        )
                    })?;
                    Ok((input.name.clone(), uri))
                })
                .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
            serde_json::to_string_pretty(&knowell_link::to_sarif(
                findings,
                env!("CARGO_PKG_VERSION"),
                &roots,
            )?)?
        }
        Format::Json => serde_json::to_string_pretty(findings)?,
        Format::Text => {
            if findings.is_empty() {
                return Ok("no findings".to_owned());
            }
            let mut text = String::new();
            for f in findings {
                let at = f.locations.first().map_or_else(String::new, |l| {
                    let lines = l
                        .range
                        .map_or_else(String::new, |r| format!(":{}", r.start()));
                    format!("{}/{}{lines}  ", l.project, l.path)
                });
                text.push_str(&format!(
                    "{at}{} [{}] {}\n",
                    f.severity.as_str(),
                    f.code,
                    f.message
                ));
            }
            text
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_on_thresholds() {
        assert!(FailOn::Error.fails(Severity::Error));
        assert!(!FailOn::Error.fails(Severity::Warning));
        assert!(FailOn::Warning.fails(Severity::Warning));
        assert!(!FailOn::Warning.fails(Severity::Info));
        assert!(!FailOn::Never.fails(Severity::Error));
    }

    #[test]
    fn sub_root_mapping() {
        let root = RepoPath::new("services/billing").unwrap();
        let p = RepoPath::new("services/billing/src/a.ts").unwrap();
        assert_eq!(
            relative_to_root(&p, Some(&root)).unwrap().as_str(),
            "src/a.ts"
        );
        let other = RepoPath::new("services/billingx/a.ts").unwrap();
        assert!(relative_to_root(&other, Some(&root)).is_none());
        assert_eq!(relative_to_root(&p, None).unwrap(), p);
    }
}
