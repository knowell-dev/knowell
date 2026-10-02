//! Helpers for hand-written JSON Schemas of string-encoded types.

use schemars::Schema;
use serde_json::{Map, Value};

/// A `"type": "string"` schema with a description, an optional regex
/// pattern and optional examples.
pub(crate) fn string_schema(description: &str, pattern: Option<&str>, examples: &[&str]) -> Schema {
    let mut map = Map::new();
    map.insert("type".into(), Value::from("string"));
    map.insert("description".into(), Value::from(description));
    if let Some(pattern) = pattern {
        map.insert("pattern".into(), Value::from(pattern));
    }
    if !examples.is_empty() {
        map.insert(
            "examples".into(),
            Value::Array(examples.iter().map(|e| Value::from(*e)).collect()),
        );
    }
    Schema::from(map)
}
