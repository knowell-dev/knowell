//! Compaction of generated JSON Schemas for `tools/list`.
//!
//! Tool definitions are read by agents on every session, so their size is
//! context the agent pays for. schemars renders a documented unit enum as a
//! `oneOf` of string constants, each with its own description; that is the
//! largest part of the raw schemas. Compaction rewrites such lists into a
//! plain `enum` (wire names are self-explanatory snake_case), and for output
//! schemas also drops descriptions, since outputs are documented in the crate
//! README and carry their own field names.
//!
//! The walker only treats keys as keywords where a schema is expected, so a
//! property that happens to be *named* `description` is never removed.

use rmcp::model::JsonObject;
use serde_json::Value;

/// Rewrites a generated input schema into the smallest form an agent still
/// needs: every `$ref` is inlined (the `$defs` table disappears), optional
/// fields lose their `null` alternative, and documentation-only or
/// server-side-validated keywords (`title`, `examples`, `format`, `pattern`,
/// descriptions of shared types, the root description, `maxLength`) are dropped. Objects
/// with properties get `additionalProperties: false`.
///
/// A property's own description is kept; the description of a *type* that is
/// inlined into it is not, so shared types never repeat their prose.
pub(crate) fn compact_input(schema: &mut JsonObject) {
    let defs = match schema.remove("$defs") {
        Some(Value::Object(defs)) => defs,
        _ => JsonObject::new(),
    };
    for keyword in ["$schema", "title", "description"] {
        schema.remove(keyword);
    }
    simplify(schema, &defs, 0);
}

/// Deepest `$ref` chain followed; input types are shallow, so a deeper chain
/// is a cycle and is left as a reference.
const MAX_INLINE_DEPTH: usize = 6;

fn simplify(schema: &mut JsonObject, defs: &JsonObject, depth: usize) {
    // Unwrapping a `$ref` or an `anyOf` can expose another one.
    for _ in 0..4 {
        let inlined = inline_ref(schema, defs, depth);
        let unwrapped = unwrap_nullable(schema);
        if !inlined && !unwrapped {
            break;
        }
    }
    collapse_const_one_of(schema);
    if let Some(Value::Array(types)) = schema.get_mut("type") {
        types.retain(|t| t.as_str() != Some("null"));
        if types.len() == 1 {
            let only = types.remove(0);
            schema.insert("type".into(), only);
        }
    }
    for keyword in ["title", "examples", "format", "pattern", "maxLength"] {
        schema.remove(keyword);
    }
    // An empty description is how a field opts out of prose in the schema
    // while keeping its Rust doc comment.
    if schema.get("description").and_then(Value::as_str) == Some("") {
        schema.remove("description");
    }
    if schema.get("minimum").and_then(Value::as_u64) == Some(0) {
        schema.remove("minimum");
    }
    if let Some(Value::Object(properties)) = schema.get_mut("properties") {
        for sub in properties.values_mut() {
            simplify_value(sub, defs, depth);
        }
    }
    for keyword in ["items", "additionalProperties"] {
        if let Some(sub) = schema.get_mut(keyword) {
            simplify_value(sub, defs, depth);
        }
    }
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(list)) = schema.get_mut(keyword) {
            for sub in list {
                simplify_value(sub, defs, depth);
            }
        }
    }
    if schema.contains_key("properties") && !schema.contains_key("additionalProperties") {
        schema.insert("additionalProperties".into(), Value::Bool(false));
    }
}

fn simplify_value(value: &mut Value, defs: &JsonObject, depth: usize) {
    if let Value::Object(schema) = value {
        simplify(schema, defs, depth + 1);
    }
}

/// Replaces `{"$ref": "#/$defs/X", ...siblings}` by the definition of `X`
/// with the siblings on top. The definition's own description and
/// `minLength` (id types) are dropped.
fn inline_ref(schema: &mut JsonObject, defs: &JsonObject, depth: usize) -> bool {
    let Some(Value::String(reference)) = schema.get("$ref") else {
        return false;
    };
    let Some(Value::Object(definition)) = reference
        .strip_prefix("#/$defs/")
        .and_then(|name| defs.get(name))
    else {
        return false;
    };
    if depth > MAX_INLINE_DEPTH {
        return false;
    }
    let mut merged = definition.clone();
    for keyword in ["description", "minLength"] {
        merged.remove(keyword);
    }
    schema.remove("$ref");
    for (key, value) in std::mem::take(schema) {
        merged.insert(key, value);
    }
    *schema = merged;
    true
}

/// `{"anyOf": [X, {"type": "null"}], ...}` becomes `X` plus the siblings:
/// optionality is already expressed by `required`.
fn unwrap_nullable(schema: &mut JsonObject) -> bool {
    let Some(Value::Array(members)) = schema.get("anyOf") else {
        return false;
    };
    let is_null = |m: &Value| m.get("type").and_then(Value::as_str) == Some("null");
    let mut rest = members.iter().filter(|m| !is_null(m));
    let (Some(Value::Object(only)), None) = (rest.next(), rest.next()) else {
        return false;
    };
    let only = only.clone();
    schema.remove("anyOf");
    for (key, value) in only {
        schema.entry(key).or_insert(value);
    }
    true
}

/// Collapses string-constant `oneOf` lists into `enum` and removes `description`, `title` and `examples`
/// keywords (never of properties with those names).
pub(crate) fn compact_output(schema: &mut JsonObject) {
    walk(schema, true);
}

fn walk(schema: &mut JsonObject, strip_docs: bool) {
    collapse_const_one_of(schema);
    if strip_docs {
        for keyword in ["description", "title", "examples"] {
            schema.remove(keyword);
        }
    }
    for map_keyword in ["properties", "$defs"] {
        if let Some(Value::Object(map)) = schema.get_mut(map_keyword) {
            for sub in map.values_mut() {
                walk_value(sub, strip_docs);
            }
        }
    }
    for keyword in ["items", "additionalProperties", "not"] {
        if let Some(sub) = schema.get_mut(keyword) {
            walk_value(sub, strip_docs);
        }
    }
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(list)) = schema.get_mut(keyword) {
            for sub in list {
                walk_value(sub, strip_docs);
            }
        }
    }
}

fn walk_value(value: &mut Value, strip_docs: bool) {
    if let Value::Object(schema) = value {
        walk(schema, strip_docs);
    }
}

/// `{"oneOf": [{"type": "string", "const": "a", ...}, ...]}` becomes
/// `{"type": "string", "enum": ["a", ...]}` when every member is a string
/// constant (other members' keywords are only documentation).
fn collapse_const_one_of(schema: &mut JsonObject) {
    let Some(Value::Array(members)) = schema.get("oneOf") else {
        return;
    };
    let constants: Option<Vec<Value>> = members
        .iter()
        .map(|member| {
            let object = member.as_object()?;
            let constant = object.get("const")?.as_str()?;
            let only_docs = object
                .keys()
                .all(|k| matches!(k.as_str(), "const" | "type" | "description" | "title"));
            only_docs.then(|| Value::from(constant))
        })
        .collect();
    let Some(constants) = constants.filter(|c| !c.is_empty()) else {
        return;
    };
    schema.remove("oneOf");
    schema.insert("type".into(), Value::from("string"));
    schema.insert("enum".into(), Value::Array(constants));
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: Value) -> JsonObject {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn collapses_documented_enums() {
        let mut schema = object(json!({
            "type": "object",
            "properties": {
                "kind": {"$ref": "#/$defs/Kind"},
                "description": {"type": "string", "description": "a field named description"}
            },
            "$defs": {
                "Kind": {
                    "description": "Kind of thing.",
                    "oneOf": [
                        {"type": "string", "const": "a", "description": "A."},
                        {"type": "string", "const": "b", "description": "B."}
                    ]
                },
                "Tagged": {
                    "oneOf": [
                        {"type": "object", "properties": {"kind": {"type": "string", "const": "x"}}}
                    ]
                }
            }
        }));
        walk(&mut schema, false);
        let kind = &schema["$defs"]["Kind"];
        assert_eq!(kind["enum"], json!(["a", "b"]));
        assert_eq!(kind["type"], "string");
        assert_eq!(kind["description"], "Kind of thing.");
        assert!(kind.get("oneOf").is_none());
        assert!(
            schema["$defs"]["Tagged"].get("oneOf").is_some(),
            "object variants stay"
        );
        assert!(schema["properties"]["description"]["description"].is_string());

        compact_output(&mut schema);
        assert!(schema["$defs"]["Kind"].get("description").is_none());
        assert!(
            schema["properties"].get("description").is_some(),
            "a property named description is kept"
        );
        assert!(
            schema["properties"]["description"]
                .get("description")
                .is_none()
        );
    }

    #[test]
    fn input_schemas_are_inlined_and_stripped() {
        let mut schema = object(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "T",
            "description": "Root prose.",
            "type": "object",
            "properties": {
                "id": {
                    "anyOf": [{"$ref": "#/$defs/Id"}, {"type": "null"}],
                    "description": "The id."
                },
                "kind": {"$ref": "#/$defs/Kind", "description": "Which."},
                "limit": {"type": ["integer", "null"], "format": "uint32", "minimum": 0, "maximum": 9},
                "list": {"type": "array", "items": {"$ref": "#/$defs/Id"}}
            },
            "required": ["kind"],
            "$defs": {
                "Id": {"description": "Long prose.", "examples": ["x"], "pattern": "^x$", "type": "string"},
                "Kind": {"oneOf": [{"const": "a"}, {"const": "b"}], "description": "Kinds."}
            }
        }));
        compact_input(&mut schema);
        assert_eq!(
            serde_json::Value::Object(schema),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": {"type": "string", "description": "The id."},
                    "kind": {"type": "string", "enum": ["a", "b"], "description": "Which."},
                    "limit": {"type": "integer", "maximum": 9},
                    "list": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["kind"]
            })
        );
    }
}
