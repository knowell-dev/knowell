//! JSON Schemas of the two configuration files, for editors and the panel.

use schemars::schema_for;
use serde_json::Value;

use crate::{EngineConfig, WorkspaceConfig};

/// JSON Schema of the engine config (`~/.knowell/config.toml`).
pub fn engine_schema() -> Value {
    unwrap_descriptions(schema_for!(EngineConfig).to_value())
}

/// JSON Schema of the workspace config (`knowell.toml`).
pub fn workspace_schema() -> Value {
    unwrap_descriptions(schema_for!(WorkspaceConfig).to_value())
}

/// Doc comments are hard-wrapped in the source; join their single line breaks
/// so descriptions read well in editors (blank-line paragraph breaks stay).
fn unwrap_descriptions(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match child {
                    Value::String(text) if key == "description" => {
                        *text = join_wrapped_lines(text);
                    }
                    other => *other = unwrap_descriptions(other.take()),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                *item = unwrap_descriptions(item.take());
            }
        }
        _ => {}
    }
    value
}

fn join_wrapped_lines(text: &str) -> String {
    text.split("\n\n")
        .map(|para| para.split('\n').collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn schemas_dir() -> PathBuf {
        PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR")
                .unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into()),
        )
        .join("..")
        .join("..")
        .join("schemas")
    }

    fn pretty(value: &Value) -> String {
        let mut text = serde_json::to_string_pretty(value).unwrap();
        text.push('\n');
        text
    }

    fn check(file: &str, value: &Value) {
        let path = schemas_dir().join(file);
        let expected = pretty(value);
        if std::env::var_os("KNOWELL_BLESS").is_some_and(|v| v == "1") {
            std::fs::create_dir_all(schemas_dir()).unwrap();
            std::fs::write(&path, expected).unwrap();
            return;
        }
        let actual = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "cannot read {}: {e}; run with KNOWELL_BLESS=1",
                path.display()
            )
        });
        // Git may check files out with CRLF on Windows.
        assert_eq!(
            actual.replace("\r\n", "\n"),
            expected,
            "{file} is out of date; regenerate with KNOWELL_BLESS=1 cargo test -p knowell-config schema"
        );
    }

    #[test]
    fn committed_engine_schema_is_current() {
        check("engine.schema.json", &engine_schema());
    }

    #[test]
    fn committed_workspace_schema_is_current() {
        check("workspace.schema.json", &workspace_schema());
    }

    #[test]
    fn schemas_carry_user_facing_descriptions() {
        let engine = pretty(&engine_schema());
        assert!(engine.contains("Reference to the API key"));
        assert!(engine.contains("loopback"));
        let ws = pretty(&workspace_schema());
        assert!(ws.contains("never assumes a default branch"));
        assert!(ws.contains("32 to 4000"));
    }

    #[test]
    fn schemas_forbid_unknown_properties() {
        assert_eq!(
            workspace_schema()["additionalProperties"],
            Value::Bool(false)
        );
        assert_eq!(engine_schema()["additionalProperties"], Value::Bool(false));
    }
}
