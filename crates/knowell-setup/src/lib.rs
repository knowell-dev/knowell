//! Setup: importing workspaces from existing layouts, connecting agent
//! clients over MCP, and generating CI integration templates.
//!
//! - [`detect`] / [`render_toml`]: propose a `knowell.toml` from submodules,
//!   language workspaces and folders of repositories.
//! - [`connect`] / [`disconnect`]: write (or remove) the MCP entry, startup
//!   instructions and hooks for Codex, Claude Code and Cursor.
//! - [`ci_init`]: GitHub, GitLab and Gitea pipeline templates.
//!
//! Every file change is idempotent and reversible, and every operation that
//! writes files has a dry-run mode that only reports diffs.

mod ci;
mod connect;
mod edit;
mod error;
mod import;

pub use ci::{CiMode, CiOptions, CiProvider, GeneratedFile, ci_init};
pub use connect::{
    Client, ConnectOptions, SERVER_NAME, Scope, connect, disconnect, instruction_block,
};
pub use edit::{BACKUP_SUFFIX, ConnectReport, FileDiff};
pub use error::SetupError;
pub use import::{
    ImportOptions, ImportPlan, ImportSource, PlannedProject, PlannedWorktree, Rename, RenameReason,
    WorktreeKind, detect, render_toml,
};
