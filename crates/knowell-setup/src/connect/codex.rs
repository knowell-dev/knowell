//! Codex: `[mcp_servers.knowell]` in `config.toml` plus an `AGENTS.md` block.
//!
//! The TOML table is written as a marker-delimited block appended to the
//! file instead of re-serialising the whole file: the `toml` crate would drop
//! the user's comments and formatting. Trade-off: the block sits at the end of
//! the file and is only replaced as a unit.

use std::path::Path;

use super::{ConnectOptions, SERVER_NAME, Scope, markdown_edit, shown};
use crate::edit::{Edit, TOML_MARKERS, has_block, plan_file, remove_block, upsert_block};
use crate::error::SetupError;

pub(super) fn plan(
    opts: &ConnectOptions,
    add: bool,
    notes: &mut Vec<String>,
) -> Result<Vec<Edit>, SetupError> {
    let (config, agents) = match opts.scope {
        Scope::User => (
            opts.home(".codex/config.toml"),
            opts.home(".codex/AGENTS.md"),
        ),
        Scope::Project => (
            opts.project(".codex/config.toml"),
            opts.project("AGENTS.md"),
        ),
    };
    if add && opts.scope == Scope::Project {
        notes.push(format!(
            "Codex loads project-scoped `.codex/config.toml` only for trusted projects; trust the project or use the user scope ({})",
            shown(&config)
        ));
    }
    let p = config.clone();
    let toml_edit = plan_file(config, move |before| apply(&p, before, opts, add))?;
    Ok(vec![toml_edit, markdown_edit(agents, add)?])
}

fn value(v: &str) -> String {
    toml::Value::String(v.to_owned()).to_string()
}

fn block(opts: &ConnectOptions) -> String {
    let args: Vec<String> = opts.args.iter().map(|a| value(a)).collect();
    let mut out = format!(
        "{}\n[mcp_servers.{SERVER_NAME}]\ncommand = {}\nargs = [{}]\n",
        TOML_MARKERS.begin,
        value(&opts.command),
        args.join(", ")
    );
    if !opts.env_names.is_empty() {
        let names: Vec<String> = opts.env_names.iter().map(|n| value(n)).collect();
        // `env_vars` forwards variables by name; no value is ever written.
        out.push_str(&format!("env_vars = [{}]\n", names.join(", ")));
    }
    out.push_str(TOML_MARKERS.end);
    out
}

fn parse(path: &Path, text: &str) -> Result<toml::Table, SetupError> {
    text.parse::<toml::Table>()
        .map_err(|_| SetupError::InvalidToml {
            path: path.to_path_buf(),
        })
}

fn apply(
    path: &Path,
    before: Option<&str>,
    opts: &ConnectOptions,
    add: bool,
) -> Result<Option<String>, SetupError> {
    let text = before.unwrap_or("");
    let existing = parse(path, text)?;
    let ours = has_block(text, TOML_MARKERS);
    if !add {
        return match (before, ours) {
            (Some(text), true) => {
                let rest = remove_block(path, text, TOML_MARKERS)?.unwrap_or_default();
                Ok(if rest.trim().is_empty() {
                    None
                } else {
                    Some(rest)
                })
            }
            (Some(text), false) => Ok(Some(text.to_owned())),
            (None, _) => Ok(None),
        };
    }
    let defined = existing
        .get("mcp_servers")
        .and_then(toml::Value::as_table)
        .is_some_and(|t| t.contains_key(SERVER_NAME));
    if defined && !ours {
        return Err(SetupError::Conflict {
            path: path.to_path_buf(),
            what: format!(
                "`[mcp_servers.{SERVER_NAME}]` already exists outside Knowell's marker block"
            ),
        });
    }
    let new = upsert_block(path, text, &block(opts), TOML_MARKERS)?;
    // A user-written `mcp_servers = { ... }` inline table would make the
    // appended table a duplicate: verify before anything is written.
    if new.parse::<toml::Table>().is_err() {
        return Err(SetupError::Conflict {
            path: path.to_path_buf(),
            what: "appending `[mcp_servers.knowell]` would make the file invalid TOML".into(),
        });
    }
    Ok(Some(new))
}
