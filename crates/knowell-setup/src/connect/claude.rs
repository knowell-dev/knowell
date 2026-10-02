//! Claude Code: `mcpServers.knowell` (project `.mcp.json` or user
//! `~/.claude.json`), a `SessionStart` hook in `settings.json`, and a
//! `CLAUDE.md` block.

use std::path::Path;

use serde_json::{Value, json};

use super::json::{Flavor, Object, apply_server, parse_object, render, server_entry};
use super::{ConnectOptions, Scope, markdown_edit};
use crate::edit::{Edit, plan_file};
use crate::error::SetupError;

/// Identifies Knowell's hook entry; `statusMessage` is a documented hook
/// field, so the marker needs no unknown keys.
const HOOK_STATUS: &str = "Knowell: loading workspace context";

pub(super) fn plan(
    opts: &ConnectOptions,
    add: bool,
    notes: &mut Vec<String>,
) -> Result<Vec<Edit>, SetupError> {
    let (mcp, settings, memory) = match opts.scope {
        Scope::User => (
            opts.home(".claude.json"),
            opts.home(".claude/settings.json"),
            opts.home(".claude/CLAUDE.md"),
        ),
        Scope::Project => (
            opts.project(".mcp.json"),
            opts.project(".claude/settings.json"),
            opts.project("CLAUDE.md"),
        ),
    };
    if add {
        notes.push(
            "the SessionStart hook runs `context --session-start`; its output is added to the session context"
                .into(),
        );
        if opts.scope == Scope::Project {
            notes.push(
                "Claude Code asks for approval before using servers from a project `.mcp.json`"
                    .into(),
            );
        }
    }
    let p = mcp.clone();
    let mcp_edit = plan_file(mcp, move |before| {
        let entry = add.then(|| server_entry(opts, Flavor::Claude));
        apply_server(&p, before, entry)
    })?;
    let p = settings.clone();
    let hook = opts.hook_command_line();
    let settings_edit = plan_file(settings, move |before| apply_hook(&p, before, &hook, add))?;
    Ok(vec![mcp_edit, settings_edit, markdown_edit(memory, add)?])
}

fn is_ours(hook: &Value) -> bool {
    hook.get("statusMessage").and_then(Value::as_str) == Some(HOOK_STATUS)
}

fn conflict(path: &Path, what: &str) -> SetupError {
    SetupError::Conflict {
        path: path.to_path_buf(),
        what: what.to_owned(),
    }
}

/// Adds or removes Knowell's hook entry in `hooks.SessionStart`.
fn apply_hook(
    path: &Path,
    before: Option<&str>,
    command: &str,
    add: bool,
) -> Result<Option<String>, SetupError> {
    let mut root = parse_object(path, before)?;
    let unchanged = || Ok(before.map(str::to_owned));

    if add && ours(&root) == [Some(command.to_owned())] {
        return unchanged();
    }

    // Drop our existing entries first; on `add` we then push a fresh one, so
    // a changed command replaces the old one instead of duplicating it.
    let mut removed = false;
    match root.get_mut("hooks") {
        None => {}
        Some(Value::Object(hooks)) => match hooks.get_mut("SessionStart") {
            None => {}
            Some(Value::Array(groups)) => {
                for group in groups.iter_mut() {
                    if let Some(Value::Array(list)) = group.get_mut("hooks") {
                        let n = list.len();
                        list.retain(|h| !is_ours(h));
                        removed |= list.len() != n;
                    }
                }
                // Only groups we emptied are dropped; user groups stay as-is.
                if removed {
                    groups.retain(|g| {
                        g.get("hooks")
                            .and_then(Value::as_array)
                            .is_none_or(|l| !l.is_empty())
                    });
                }
            }
            Some(_) => return Err(conflict(path, "`hooks.SessionStart` is not an array")),
        },
        Some(_) => return Err(conflict(path, "`hooks` is not an object")),
    }

    if add {
        let hooks = root
            .entry("hooks")
            .or_insert_with(|| Value::Object(Object::new()));
        let Value::Object(hooks) = hooks else {
            return Err(conflict(path, "`hooks` is not an object"));
        };
        let groups = hooks
            .entry("SessionStart")
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(groups) = groups else {
            return Err(conflict(path, "`hooks.SessionStart` is not an array"));
        };
        groups.push(json!({
            "hooks": [{
                "type": "command",
                "command": command,
                "statusMessage": HOOK_STATUS,
            }]
        }));
    } else {
        if !removed {
            return unchanged();
        }
        if let Some(Value::Object(hooks)) = root.get_mut("hooks") {
            if hooks
                .get("SessionStart")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                hooks.remove("SessionStart");
            }
            if hooks.is_empty() {
                root.remove("hooks");
            }
        }
    }
    if root.is_empty() {
        return Ok(None);
    }
    Ok(Some(render(root)))
}

/// Commands of all Knowell hook entries found (`None` for an entry without
/// a command string).
fn ours(root: &Object) -> Vec<Option<String>> {
    root.get("hooks")
        .and_then(|h| h.get("SessionStart"))
        .and_then(Value::as_array)
        .map(|groups| {
            groups
                .iter()
                .filter_map(|g| g.get("hooks").and_then(Value::as_array))
                .flatten()
                .filter(|h| is_ours(h))
                .map(|h| h.get("command").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}
