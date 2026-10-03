//! The `know` command: Knowell's command-line interface.
//!
//! Output goes to stdout through [`Output`]; diagnostics go to stderr through
//! `tracing` (filter with `KNOWELL_LOG`, e.g. `KNOWELL_LOG=debug`, or with
//! `-v` / `-q`).
//!
//! Exit codes: `0` success, `1` findings, a failed check or a refused change,
//! `2` operational error (I/O, database unreachable, invalid arguments).

mod backup_cmd;
mod check_cmd;
mod ci_cmd;
mod config_cmd;
mod connect_cmd;
mod context_cmd;
mod db;
mod doctor_cmd;
mod engine_file;
mod env;
mod eval_cmd;
mod eval_hybrid;
mod fsutil;
mod graph_output;
mod impact_cmd;
mod index_cmd;
mod init_cmd;
mod local_engine;
mod login_cmd;
mod mcp_cmd;
mod output;
mod project_cmd;
mod registry;
mod search_cmd;
mod secrets_cmd;
mod serve_cmd;
mod status_cmd;
mod token_cmd;
mod tools;
mod trace_cmd;
mod workspace_cmd;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Args, Parser, Subcommand};

use crate::env::Env;
use crate::output::Output;

/// Knowell — semantic code intelligence and shared memory for AI coding agents.
#[derive(Debug, Parser)]
#[command(name = "know", version, about, long_about = None, propagate_version = true)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(subcommand)]
    command: Command,
}

/// Options accepted by every command.
#[derive(Debug, Clone, Args)]
pub(crate) struct GlobalArgs {
    /// Engine configuration file [default: $KNOWELL_HOME/config.toml, with
    /// KNOWELL_HOME defaulting to ~/.knowell].
    #[arg(long = "config", value_name = "ENGINE_TOML", global = true)]
    pub(crate) engine_config: Option<PathBuf>,
    /// Workspace file [default: ./knowell.toml, searched upward from the
    /// current directory].
    #[arg(long = "workspace", value_name = "KNOWELL_TOML", global = true)]
    pub(crate) workspace_file: Option<PathBuf>,
    /// More log output on stderr (-v debug, -vv trace).
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count, global = true)]
    verbose: u8,
    /// Only errors on stderr.
    #[arg(short = 'q', long = "quiet", global = true, conflicts_with = "verbose")]
    quiet: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create the engine configuration and set up its database.
    Init(init_cmd::InitArgs),
    /// Import, register and list workspaces.
    #[command(subcommand)]
    Workspace(workspace_cmd::WorkspaceCommand),
    /// Add projects to the workspace file.
    #[command(subcommand)]
    Project(project_cmd::ProjectCommand),
    /// Connect an agent client (MCP entry, start-up instructions, hooks).
    Connect(connect_cmd::ConnectArgs),
    /// Remove what `know connect` added for an agent client.
    Disconnect(connect_cmd::ConnectArgs),
    /// Generate CI integration files.
    #[command(subcommand)]
    Ci(ci_cmd::CiCommand),
    /// Check the installation: configuration, database, pgvector, git, panel,
    /// providers and agent clients.
    Doctor(doctor_cmd::DoctorArgs),
    /// Run the server: REST API, panel and MCP over Streamable HTTP.
    Serve(serve_cmd::ServeArgs),
    /// Serve MCP over stdin/stdout for an agent client.
    Mcp(mcp_cmd::McpArgs),
    /// Check contracts and rules across all projects (no API key needed);
    /// text, JSON or SARIF output for CI.
    Check(check_cmd::CheckArgs),
    /// Index the selected local workspace and report completion of every tier.
    Index(index_cmd::IndexArgs),
    /// Search the selected local workspace's existing index with versioned evidence.
    Search(search_cmd::SearchArgs),
    /// Trace evidenced relations in the selected local workspace's existing index.
    Trace(trace_cmd::TraceArgs),
    /// Analyze a symbol, file or committed diff using the local source graph.
    Impact(impact_cmd::ImpactArgs),
    /// Report local index freshness, tier states and embedding coverage.
    Status(status_cmd::StatusArgs),
    /// Back up the managed database.
    Backup(backup_cmd::BackupArgs),
    /// Restore a backup into the managed PostgreSQL.
    Restore(backup_cmd::RestoreArgs),
    /// Connect this machine to a hub (role `edge`).
    Login(login_cmd::LoginArgs),
    /// Administer this installation's database-backed API tokens.
    #[command(subcommand)]
    Token(token_cmd::TokenCommand),
    /// Print workspace context for an agent session.
    Context(context_cmd::ContextArgs),
    /// Validate configuration files and print JSON Schemas.
    #[command(subcommand)]
    Config(config_cmd::ConfigCommand),
    /// Inspect what the secret boundary excludes and redacts in a directory.
    #[command(subcommand)]
    Secrets(secrets_cmd::SecretsCommand),
    /// Generate synthetic evaluation workspaces and measure retrieval quality.
    #[command(subcommand)]
    Eval(eval_cmd::EvalCommand),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli.global);
    let mut out = Output::stdout();
    // The session hook must never fail and must not wait for anything.
    if let Command::Context(args) = &cli.command {
        return context_cmd::run(args, &cli.global, &mut out);
    }
    let env = match Env::from_globals(&cli.global) {
        Ok(env) => env,
        Err(err) => {
            tracing::error!("{err:#}");
            return ExitCode::from(2);
        }
    };
    let result = match cli.command {
        Command::Init(args) => init_cmd::run(args, &env, &mut out),
        Command::Workspace(cmd) => workspace_cmd::run(cmd, &env, &mut out),
        Command::Project(cmd) => project_cmd::run(cmd, &env, &mut out),
        Command::Connect(args) => connect_cmd::run(args, true, &env, &mut out),
        Command::Disconnect(args) => connect_cmd::run(args, false, &env, &mut out),
        Command::Ci(cmd) => ci_cmd::run(cmd, &mut out),
        Command::Doctor(args) => doctor_cmd::run(args, &env, &mut out),
        Command::Serve(args) => serve_cmd::run(args, &env, &mut out),
        Command::Mcp(args) => mcp_cmd::run(args, &env),
        Command::Check(args) => check_cmd::run(args, &env, &mut out),
        Command::Index(args) => index_cmd::run(args, &env, &mut out),
        Command::Search(args) => search_cmd::run(args, &env, &mut out),
        Command::Trace(args) => trace_cmd::run(args, &env, &mut out),
        Command::Impact(args) => impact_cmd::run(args, &env, &mut out),
        Command::Status(args) => status_cmd::run(args, &env, &mut out),
        Command::Backup(args) => backup_cmd::backup(args, &env, &mut out),
        Command::Restore(args) => backup_cmd::restore(args, &env, &mut out),
        Command::Login(args) => login_cmd::run(args, &env, &mut out),
        Command::Token(args) => token_cmd::run(args, &env, &mut out),
        Command::Context(_) => Ok(ExitCode::SUCCESS),
        Command::Config(cmd) => config_cmd::run(cmd, &mut out),
        Command::Secrets(cmd) => secrets_cmd::run(cmd, &mut out),
        Command::Eval(cmd) => eval_cmd::run(cmd, &mut out),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            tracing::error!("{err:#}");
            ExitCode::from(2)
        }
    }
}

fn init_tracing(global: &GlobalArgs) {
    use tracing_subscriber::EnvFilter;
    let default = if global.quiet {
        "error"
    } else {
        match global.verbose {
            0 => "info,tantivy=warn",
            1 => "debug,tantivy=info,sqlx=info,hyper=info,h2=info,rustls=info",
            _ => "trace",
        }
    };
    let filter = EnvFilter::try_from_env("KNOWELL_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    use std::io::IsTerminal as _;
    let ansi = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ansi)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .init();
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    #[test]
    fn command_tree_is_consistent() {
        super::Cli::command().debug_assert();
    }
}
