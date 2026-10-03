//! `know workspace import|add|list`.

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use clap::Subcommand;
use knowell_config::ResolvedWorkspace;
use knowell_core::{Name, TrackTarget};
use knowell_setup::{ImportOptions, ImportPlan, RenameReason, WorktreeKind};
use knowell_store::hierarchy;
use knowell_store::{PgConnection, SourceKind, Store, views};

use crate::db;
use crate::env::{self, Env, WORKSPACE_FILE};
use crate::fsutil;
use crate::output::Output;
use crate::registry::{Entry, Registry};

#[derive(Debug, Subcommand)]
pub(crate) enum WorkspaceCommand {
    /// Propose a knowell.toml from the repositories in a directory
    /// (submodules, go.work, pnpm/npm/Cargo workspaces, folders of repositories).
    Import {
        /// Directory to inspect; knowell.toml is written there.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Workspace name [default: the directory name].
        #[arg(long)]
        name: Option<String>,
        /// Follow each repository's currently checked-out branch.
        #[arg(long)]
        track_current: bool,
        /// Print the file instead of writing it.
        #[arg(long)]
        dry_run: bool,
        /// Replace an existing knowell.toml.
        #[arg(long)]
        force: bool,
    },
    /// Register a workspace file with the engine: organization, workspace,
    /// projects, sources and views (nothing is indexed).
    Add {
        /// Workspace file [default: --workspace, or knowell.toml searched upward].
        file: Option<PathBuf>,
        /// Organization (tenant) to register into.
        #[arg(long, default_value = "local")]
        organization: Name,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the workspaces registered with the engine.
    List {
        /// Organization (tenant) to list.
        #[arg(long, default_value = "local")]
        organization: Name,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn run(cmd: WorkspaceCommand, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    match cmd {
        WorkspaceCommand::Import {
            dir,
            name,
            track_current,
            dry_run,
            force,
        } => import(&dir, name, track_current, dry_run, force, out),
        WorkspaceCommand::Add {
            file,
            organization,
            json,
        } => {
            let file = match file {
                Some(file) => env::absolute(&file)?,
                None => env.require_workspace()?,
            };
            add(&file, &organization, json, env, out)
        }
        WorkspaceCommand::List { organization, json } => list(&organization, json, env, out),
    }
}

// ---------------------------------------------------------------- import

fn import(
    dir: &Path,
    name: Option<String>,
    track_current: bool,
    dry_run: bool,
    force: bool,
    out: &mut Output,
) -> anyhow::Result<ExitCode> {
    let opts = ImportOptions {
        workspace_name: name,
        track_current,
        ..ImportOptions::default()
    };
    let mut plan = knowell_setup::detect(dir, &opts)?;
    if plan.projects.is_empty() {
        tracing::error!(
            "no projects found in {} (no submodules, language workspaces or child git repositories); add them with `know project add`",
            dir.display()
        );
        return Ok(ExitCode::FAILURE);
    }
    report_plan(&plan, dry_run, out)?;

    let interactive = !dry_run && std::io::stdin().is_terminal();
    if interactive && !plan.projects_needing_track().is_empty() {
        ask_tracks(&mut plan)?;
    }

    let text = knowell_setup::render_toml(&plan);
    // The renderer promises a parseable file; check before writing anything.
    knowell_config::parse_workspace(&text)
        .map_err(|e| anyhow::anyhow!("internal error: the proposed workspace is invalid: {e}"))?;
    let missing: Vec<String> = plan
        .projects_needing_track()
        .iter()
        .map(|p| p.name.to_string())
        .collect();

    if dry_run {
        out.line(text.trim_end())?;
    } else {
        let target = dir.join(WORKSPACE_FILE);
        match fsutil::read_optional(&target)? {
            Some(existing) if existing == text => {
                out.line(format!("{} is up to date", target.display()))?;
            }
            Some(_) if !force => {
                out.flush()?;
                tracing::error!(
                    "{} already exists; pass --force to replace it (or --dry-run to compare)",
                    target.display()
                );
                return Ok(ExitCode::FAILURE);
            }
            _ => {
                fsutil::write_atomic(&target, &text)?;
                out.line(format!("wrote {}", target.display()))?;
            }
        }
    }
    out.flush()?;

    if !missing.is_empty() {
        tracing::error!(
            "no track target for: {}. Knowell never guesses a branch: set `track` in {WORKSPACE_FILE} (e.g. track = \"branch:main\"), or re-run with --track-current",
            missing.join(", ")
        );
        return Ok(ExitCode::FAILURE);
    }
    if !dry_run {
        out.line("next: `know workspace add` registers it with the engine")?;
        out.flush()?;
    }
    Ok(ExitCode::SUCCESS)
}

fn report_plan(plan: &ImportPlan, dry_run: bool, out: &mut Output) -> anyhow::Result<()> {
    let mut lines = vec![format!(
        "workspace `{}`: {} project(s)",
        plan.workspace_name,
        plan.projects.len()
    )];
    for p in &plan.projects {
        let root = p
            .root
            .as_ref()
            .map(|r| format!(" root {r}"))
            .unwrap_or_default();
        let track = p
            .track
            .as_ref()
            .map_or_else(|| "track: (not set)".to_owned(), |t| format!("track {t}"));
        lines.push(format!(
            "  {:<24} {}{root}  {track}  [{}]",
            p.name.as_str(),
            p.path,
            p.source.label()
        ));
    }
    for w in &plan.worktrees {
        let how = match w.kind {
            WorktreeKind::Pattern => "worktree folder",
            WorktreeKind::LinkedWorktree => "linked worktree",
        };
        lines.push(format!("  worktree {} ({how}; not a project)", w.path));
    }
    for r in &plan.renames {
        let why = match r.reason {
            RenameReason::Slugified => "made a valid name",
            RenameReason::Collision => "name taken",
        };
        lines.push(format!(
            "  renamed {}: `{}` -> `{}` ({why})",
            r.location, r.original, r.assigned
        ));
    }
    for w in &plan.warnings {
        lines.push(format!("  warning: {w}"));
    }
    if dry_run {
        // stdout carries the file itself.
        for line in lines {
            tracing::info!("{line}");
        }
    } else {
        for line in lines {
            out.line(line)?;
        }
        out.flush()?;
    }
    Ok(())
}

/// Asks for each missing track target on the terminal; an empty answer
/// leaves it for later.
fn ask_tracks(plan: &mut ImportPlan) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stderr = std::io::stderr();
    writeln!(
        stderr,
        "Knowell never guesses which ref to follow. Enter a track target per project\n(branch:<name>, remote:origin/<name>, tag:<name>, commit:<sha>, worktree); empty = decide later."
    )?;
    for project in plan.projects.iter_mut().filter(|p| p.track.is_none()) {
        for _attempt in 0..3 {
            write!(stderr, "track for {} ({}): ", project.name, project.path)?;
            stderr.flush()?;
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                return Ok(()); // end of input
            }
            let answer = line.trim();
            if answer.is_empty() {
                break;
            }
            match TrackTarget::from_str(answer) {
                Ok(target) => {
                    project.track = Some(target);
                    break;
                }
                Err(err) => writeln!(stderr, "  {err}")?,
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- add

/// Counts of what `add` created versus found.
#[derive(Debug, Default)]
struct Registered {
    projects_new: usize,
    sources_new: usize,
    views_new: usize,
}

fn load_resolved(file: &Path) -> anyhow::Result<Result<ResolvedWorkspace, String>> {
    let config = match knowell_config::load_workspace(file) {
        Ok(config) => config,
        Err(err) => return Ok(Err(err.to_string())),
    };
    let base = env::parent_dir(file)?;
    Ok(config
        .resolve(&base)
        .map_err(|issues| format!("{}: {issues}", file.display())))
}

fn add(
    file: &Path,
    organization: &Name,
    json: bool,
    env: &Env,
    out: &mut Output,
) -> anyhow::Result<ExitCode> {
    let resolved = match load_resolved(file)? {
        Ok(resolved) => resolved,
        Err(message) => {
            tracing::error!("{message}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let engine = env.require_engine()?;
    let issues = resolved.check_against(&engine);
    if !issues.is_empty() {
        for issue in &issues {
            tracing::error!("{issue}");
        }
        return Ok(ExitCode::FAILURE);
    }
    for project in &resolved.projects {
        if !project.path.is_dir() {
            tracing::error!(
                "project `{}`: {} is not a directory",
                project.name,
                project.path.display()
            );
            return Ok(ExitCode::FAILURE);
        }
    }

    let rt = db::runtime()?;
    let outcome = rt.block_on(async {
        let store = db::connect(env, &engine, Duration::from_secs(15), 2).await?;
        store
            .validate_schema()
            .await
            .context("cannot apply the database migrations")?;
        let result = register(&store, organization, &resolved).await;
        store.close().await;
        result
    });
    let registered = match outcome {
        Ok(Ok(registered)) => registered,
        Ok(Err(conflict)) => {
            tracing::error!("{conflict}");
            return Ok(ExitCode::FAILURE);
        }
        Err(err) => return Err(err),
    };

    let mut registry = Registry::load(&env.home)?;
    registry.upsert(Entry {
        organization: organization.clone(),
        workspace: resolved.name.clone(),
        file: file.to_path_buf(),
    });
    registry.save(&env.home)?;

    if json {
        let value = serde_json::json!({
            "organization": organization,
            "workspace": resolved.name,
            "file": file,
            "projects": resolved.projects.len(),
            "projects_created": registered.projects_new,
            "sources_created": registered.sources_new,
            "views_created": registered.views_new,
            "indexed": false,
        });
        out.line(serde_json::to_string_pretty(&value)?)?;
    } else {
        out.line(format!(
            "workspace `{}` registered in organization `{organization}` from {}",
            resolved.name,
            file.display()
        ))?;
        out.line(format!(
            "  {} project(s) ({} new), {} new source(s), {} new view(s); nothing indexed yet",
            resolved.projects.len(),
            registered.projects_new,
            registered.sources_new,
            registered.views_new
        ))?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

/// Creates what is missing in one transaction. The inner `Err` is a
/// conflict with what is already registered (exit 1); nothing is changed then.
async fn register(
    store: &Store,
    organization: &Name,
    ws: &ResolvedWorkspace,
) -> anyhow::Result<Result<Registered, String>> {
    let mut tx = store.begin().await?;
    let conn: &mut PgConnection = &mut tx;
    let mut counts = Registered::default();
    let org = match hierarchy::find_organization(conn, organization).await? {
        Some(org) => org,
        None => hierarchy::create_organization(conn, organization).await?,
    };
    let workspace = match hierarchy::find_workspace(conn, org.id, &ws.name).await? {
        Some(w) => w,
        None => hierarchy::create_workspace(conn, org.id, &ws.name).await?,
    };
    for project in &ws.projects {
        let location = source_location(&project.path)?;
        let kind = if project.path.join(".git").exists() {
            SourceKind::Git
        } else {
            SourceKind::Directory
        };
        let source = match hierarchy::find_source(conn, org.id, &location).await? {
            Some(s) => s,
            None => {
                counts.sources_new += 1;
                hierarchy::create_source(conn, org.id, kind, &location).await?
            }
        };
        let stored = match hierarchy::find_project(conn, workspace.id, &project.name).await? {
            Some(existing) => {
                if existing.source != source.id || existing.root != project.root {
                    return Ok(Err(format!(
                        "project `{}` is already registered with another location; rename it in {WORKSPACE_FILE} or remove the old registration",
                        project.name
                    )));
                }
                existing
            }
            None => {
                counts.projects_new += 1;
                hierarchy::create_project(
                    conn,
                    workspace.id,
                    source.id,
                    &project.name,
                    project.root.as_ref(),
                )
                .await?
            }
        };
        let target = &project.track.value;
        if views::find_view(conn, stored.id, target).await?.is_none() {
            counts.views_new += 1;
            views::create_view(conn, stored.id, target).await?;
        }
    }
    tx.commit()
        .await
        .context("cannot commit the registration")?;
    Ok(Ok(counts))
}

/// The source location: the absolute directory of the repository.
fn source_location(path: &Path) -> anyhow::Result<String> {
    let abs = env::absolute(path)?;
    abs.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{} is not valid UTF-8", abs.display()))
}

// ---------------------------------------------------------------- list

fn list(organization: &Name, json: bool, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let engine = env.require_engine()?;
    let registry = Registry::load(&env.home)?;
    let rt = db::runtime()?;
    let listing = rt.block_on(async {
        let store = db::connect(env, &engine, Duration::from_secs(15), 2).await?;
        let result = collect(&store, organization).await;
        store.close().await;
        result
    })?;

    if json {
        let value = serde_json::json!({
            "organization": organization,
            "workspaces": listing.iter().map(|w| serde_json::json!({
                "name": w.name,
                "file": registry.file_of(organization, &w.name),
                "projects": w.projects.iter().map(|p| serde_json::json!({
                    "name": p.name,
                    "source": p.source,
                    "root": p.root,
                    "views": p.views,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        out.line(serde_json::to_string_pretty(&value)?)?;
    } else if listing.is_empty() {
        out.line(format!(
            "no workspaces registered in organization `{organization}`; register one with `know workspace add`"
        ))?;
    } else {
        for w in &listing {
            let file = registry
                .file_of(organization, &w.name)
                .map_or_else(|| "(file unknown)".to_owned(), |f| f.display().to_string());
            out.line(format!("workspace {}  {file}", w.name))?;
            for p in &w.projects {
                let root = p
                    .root
                    .as_ref()
                    .map(|r| format!(" [{r}]"))
                    .unwrap_or_default();
                out.line(format!(
                    "  {:<24} {}{root}  {}",
                    p.name,
                    p.source,
                    p.views.join(", ")
                ))?;
            }
        }
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

struct ListedWorkspace {
    name: Name,
    projects: Vec<ListedProject>,
}

struct ListedProject {
    name: String,
    source: String,
    root: Option<String>,
    views: Vec<String>,
}

async fn collect(store: &Store, organization: &Name) -> anyhow::Result<Vec<ListedWorkspace>> {
    let mut conn = store.acquire().await?;
    let Some(org) = hierarchy::find_organization(&mut conn, organization).await? else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for w in hierarchy::list_workspaces(&mut conn, org.id).await? {
        let mut projects = Vec::new();
        for p in hierarchy::list_projects(&mut conn, w.id).await? {
            let source = hierarchy::get_source(&mut conn, p.source)
                .await?
                .map_or_else(|| "(missing source)".to_owned(), |s| s.location);
            let views = views::list_views(&mut conn, p.id)
                .await?
                .into_iter()
                .map(|v| v.target.to_string())
                .collect();
            projects.push(ListedProject {
                name: p.name.to_string(),
                source,
                root: p.root.map(|r| r.to_string()),
                views,
            });
        }
        result.push(ListedWorkspace {
            name: w.name,
            projects,
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_workspace_file_is_reported_not_panicked() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_resolved(&dir.path().join("knowell.toml")).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn unresolved_tracks_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("knowell.toml");
        std::fs::write(
            &file,
            "version = 1\n[workspace]\nname = \"w\"\n[[project]]\nname = \"a\"\npath = \"a\"\n",
        )
        .unwrap();
        let message = load_resolved(&file).unwrap().unwrap_err();
        assert!(message.contains("track"), "{message}");
    }
}
