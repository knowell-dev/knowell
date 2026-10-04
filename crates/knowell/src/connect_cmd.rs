//! `know connect` / `know disconnect`: agent client configuration through
//! `knowell-setup` (MCP entry, start-up instructions, Claude Code hook).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, ValueEnum};
use knowell_setup::{Client, ConnectOptions, ConnectReport, Scope, SetupError};

use crate::env::{self, Env};
use crate::output::Output;

#[derive(Debug, Args)]
pub(crate) struct ConnectArgs {
    /// Agent client.
    #[arg(value_enum)]
    client: ClientArg,
    /// `project`: files in the project directory (shared when committed);
    /// `user`: the client's configuration in your home directory.
    #[arg(long, value_enum, default_value_t = ScopeArg::Project)]
    scope: ScopeArg,
    /// Show the diffs without writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Project directory [default: the directory of knowell.toml, else the
    /// current directory].
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,
    /// Command that starts Knowell in the client's configuration.
    #[arg(long, default_value = "know")]
    command: String,
    /// Environment variable NAME (never a value) the client passes to the
    /// server, e.g. KNOWELL_HUB_TOKEN (repeatable).
    #[arg(long = "env-name", value_name = "NAME")]
    env_names: Vec<String>,
    /// Print the report as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ClientArg {
    /// OpenAI Codex.
    Codex,
    /// Claude Code.
    Claude,
    /// Cursor.
    Cursor,
}

impl From<ClientArg> for Client {
    fn from(arg: ClientArg) -> Self {
        match arg {
            ClientArg::Codex => Client::Codex,
            ClientArg::Claude => Client::Claude,
            ClientArg::Cursor => Client::Cursor,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ScopeArg {
    /// The project directory.
    Project,
    /// Your home directory.
    User,
}

impl From<ScopeArg> for Scope {
    fn from(arg: ScopeArg) -> Self {
        match arg {
            ScopeArg::Project => Scope::Project,
            ScopeArg::User => Scope::User,
        }
    }
}

/// The project directory `connect` writes to.
pub(crate) fn project_dir(
    explicit: Option<&std::path::Path>,
    env: &Env,
) -> anyhow::Result<PathBuf> {
    if let Some(dir) = explicit {
        return env::absolute(dir);
    }
    match env.find_workspace()? {
        Some(file) => env::parent_dir(&file),
        None => env::absolute(std::path::Path::new(".")),
    }
}

/// Applies the selected global routing to a launcher without serializing environment values.
/// Shared by connection writes and the doctor's read-only connection check.
pub(crate) fn configure_launcher(env: &Env, opts: &mut ConnectOptions) -> anyhow::Result<()> {
    opts.args = env.connection_args()?;
    opts.args
        .extend(["mcp", "--output-mode", "source"].map(str::to_owned));
    if env.has_home_override() && !opts.env_names.iter().any(|name| name == "KNOWELL_HOME") {
        opts.env_names.push("KNOWELL_HOME".to_owned());
    }
    Ok(())
}

pub(crate) fn run(
    args: ConnectArgs,
    connect: bool,
    env: &Env,
    out: &mut Output,
) -> anyhow::Result<ExitCode> {
    let project = project_dir(args.dir.as_deref(), env)?;
    let mut opts = ConnectOptions::new(env::user_home()?, &project);
    opts.scope = args.scope.into();
    opts.dry_run = args.dry_run;
    opts.command = args.command.clone();
    opts.env_names = args.env_names.clone();
    if connect {
        configure_launcher(env, &mut opts)?;
    }
    let client: Client = args.client.into();
    let result = if connect {
        knowell_setup::connect(client, &opts)
    } else {
        knowell_setup::disconnect(client, &opts)
    };
    let mut report = match result {
        Ok(report) => report,
        // Refusals: the files hold something Knowell must not touch.
        Err(
            err @ (SetupError::Conflict { .. }
            | SetupError::InvalidJson { .. }
            | SetupError::InvalidToml { .. }),
        ) => {
            tracing::error!("{err}");
            return Ok(ExitCode::FAILURE);
        }
        Err(err) => return Err(err.into()),
    };
    if connect && opts.env_names.iter().any(|name| name == "KNOWELL_HOME") {
        report.notes.push(
            "the client and its session hook must inherit KNOWELL_HOME; only its name is forwarded, never its value"
                .to_owned(),
        );
    }
    print_report(&args, client, connect, &report, out)?;
    Ok(ExitCode::SUCCESS)
}

fn print_report(
    args: &ConnectArgs,
    client: Client,
    connect: bool,
    report: &ConnectReport,
    out: &mut Output,
) -> anyhow::Result<()> {
    let scope = match args.scope {
        ScopeArg::Project => "project",
        ScopeArg::User => "user",
    };
    if args.json {
        let value = serde_json::json!({
            "client": client.as_str(),
            "action": if connect { "connect" } else { "disconnect" },
            "scope": scope,
            "dry_run": args.dry_run,
            "changed_files": report.changed_files,
            "backups": report.backups,
            "notes": report.notes,
            "diffs": report.diffs.iter().map(|d| serde_json::json!({
                "path": d.path,
                "diff": d.diff,
            })).collect::<Vec<_>>(),
        });
        out.line(serde_json::to_string_pretty(&value)?)?;
        out.flush()?;
        return Ok(());
    }
    if args.dry_run {
        for diff in &report.diffs {
            out.line(diff.diff.trim_end())?;
        }
        out.line(format!(
            "dry run: {} file(s) would change; nothing was written",
            report.changed_files.len()
        ))?;
    } else {
        let verb = if connect { "connected" } else { "disconnected" };
        out.line(format!("{} {verb} ({scope} scope)", client.as_str()))?;
        for path in &report.changed_files {
            out.line(format!("  changed {}", path.display()))?;
        }
        for path in &report.backups {
            out.line(format!("  backup  {}", path.display()))?;
        }
    }
    for note in &report.notes {
        out.line(format!("note: {note}"))?;
    }
    out.flush()?;
    Ok(())
}
