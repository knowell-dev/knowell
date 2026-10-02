//! JSON merge helpers for client configuration files.
//!
//! Files are parsed, merged and pretty-printed (two-space indent, trailing
//! newline). Trade-off: the `serde_json` map is ordered by key, so a rewritten
//! file may list keys alphabetically; values and other servers are preserved.
//! JSON with comments is rejected rather than rewritten lossy.

use std::path::Path;

use serde_json::{Map, Value, json};

use super::{ConnectOptions, SERVER_NAME};
use crate::error::SetupError;

pub(crate) type Object = Map<String, Value>;

/// Parses a JSON object; empty or whitespace-only text is an empty object.
pub(crate) fn parse_object(path: &Path, text: Option<&str>) -> Result<Object, SetupError> {
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        return Ok(Object::new());
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(SetupError::InvalidJson {
            path: path.to_path_buf(),
            line: 0,
            column: 0,
        }),
        Err(e) => Err(SetupError::InvalidJson {
            path: path.to_path_buf(),
            line: e.line(),
            column: e.column(),
        }),
    }
}

/// Pretty-prints an object with a trailing newline.
pub(crate) fn render(map: Object) -> String {
    let mut s = serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_else(|_| "{}".into());
    s.push('\n');
    s
}

fn conflict(path: &Path, what: &str) -> SetupError {
    SetupError::Conflict {
        path: path.to_path_buf(),
        what: what.to_owned(),
    }
}

/// How the client expects environment pass-through to be written.
#[derive(Clone, Copy)]
pub(crate) enum Flavor {
    /// Claude Code: `"type": "stdio"`, `${NAME}` expansion.
    Claude,
    /// Cursor: no `type`, `${env:NAME}` interpolation.
    Cursor,
}

/// The `mcpServers.knowell` value.
pub(crate) fn server_entry(opts: &ConnectOptions, flavor: Flavor) -> Value {
    let mut entry = Object::new();
    if matches!(flavor, Flavor::Claude) {
        entry.insert("type".into(), json!("stdio"));
    }
    entry.insert("command".into(), json!(opts.command));
    entry.insert("args".into(), json!(opts.args));
    if !opts.env_names.is_empty() {
        let env: Object = opts
            .env_names
            .iter()
            .map(|n| {
                let v = match flavor {
                    Flavor::Claude => format!("${{{n}}}"),
                    Flavor::Cursor => format!("${{env:{n}}}"),
                };
                (n.clone(), Value::String(v))
            })
            .collect();
        entry.insert("env".into(), Value::Object(env));
    }
    Value::Object(entry)
}

/// Adds or removes `mcpServers.knowell`; returns `None` when the resulting
/// file would be empty (so it can be deleted).
pub(crate) fn apply_server(
    path: &Path,
    before: Option<&str>,
    entry: Option<Value>,
) -> Result<Option<String>, SetupError> {
    let mut root = parse_object(path, before)?;
    match entry {
        Some(entry) => {
            let servers = root
                .entry("mcpServers")
                .or_insert_with(|| Value::Object(Object::new()));
            let Value::Object(servers) = servers else {
                return Err(conflict(path, "`mcpServers` is not an object"));
            };
            if servers.get(SERVER_NAME) == Some(&entry) {
                // Keep the user's formatting when nothing changes.
                return Ok(before.map(str::to_owned));
            }
            servers.insert(SERVER_NAME.into(), entry);
        }
        None => {
            let now_empty = match root.get_mut("mcpServers") {
                Some(Value::Object(servers)) => {
                    if servers.remove(SERVER_NAME).is_none() {
                        return Ok(before.map(str::to_owned));
                    }
                    servers.is_empty()
                }
                Some(_) => return Err(conflict(path, "`mcpServers` is not an object")),
                None => return Ok(before.map(str::to_owned)),
            };
            if now_empty {
                root.remove("mcpServers");
            }
        }
    }
    Ok(if root.is_empty() {
        None
    } else {
        Some(render(root))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_malformed_and_non_object() {
        let p = Path::new("c.json");
        assert!(matches!(
            parse_object(p, Some("{ \"a\": ")),
            Err(SetupError::InvalidJson { .. })
        ));
        assert!(parse_object(p, Some("[1]")).is_err());
        assert!(parse_object(p, Some("// c\n{}")).is_err());
        assert!(parse_object(p, Some("  ")).unwrap().is_empty());
        assert!(parse_object(p, None).unwrap().is_empty());
    }

    #[test]
    fn error_does_not_echo_content() {
        let e = parse_object(Path::new("c.json"), Some("{\"k\": SECRETVALUE}")).unwrap_err();
        assert!(!e.to_string().contains("SECRETVALUE"));
    }
}
