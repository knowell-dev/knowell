//! A small JSON Schema (draft 2020-12) checker for the keywords schemars
//! emits. It is enough to prove that advertised schemas are well formed and
//! that structured outputs and sample inputs conform; `pattern` and `format`
//! are not evaluated (no regex engine in the test dependencies).

use serde_json::{Map, Value};

const TYPES: &[&str] = &[
    "null", "boolean", "object", "array", "number", "integer", "string",
];

const KNOWN_KEYWORDS: &[&str] = &[
    "$schema",
    "$ref",
    "$defs",
    "$id",
    "title",
    "description",
    "examples",
    "default",
    "deprecated",
    "readOnly",
    "writeOnly",
    "type",
    "enum",
    "const",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
];

/// Structural problems in a schema document (unresolvable `$ref`s, unknown
/// types or keywords, `required` names without a property).
pub(crate) fn check_well_formed(root: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    walk(root, root, "#", &mut errors);
    errors
}

fn walk(root: &Value, schema: &Value, path: &str, errors: &mut Vec<String>) {
    let object = match schema {
        Value::Bool(_) => return,
        Value::Object(object) => object,
        other => {
            errors.push(format!(
                "{path}: schema must be an object or boolean, got {other}"
            ));
            return;
        }
    };
    for key in object.keys() {
        if !KNOWN_KEYWORDS.contains(&key.as_str()) {
            errors.push(format!("{path}: unexpected keyword `{key}`"));
        }
    }
    if let Some(reference) = object.get("$ref") {
        match reference.as_str() {
            Some(r) if resolve(root, r).is_some() => {}
            _ => errors.push(format!("{path}: unresolvable $ref {reference}")),
        }
    }
    match object.get("type") {
        None => {}
        Some(Value::String(t)) if TYPES.contains(&t.as_str()) => {}
        Some(Value::Array(ts))
            if ts
                .iter()
                .all(|t| t.as_str().is_some_and(|t| TYPES.contains(&t))) => {}
        Some(other) => errors.push(format!("{path}: invalid type {other}")),
    }
    if let Some(required) = object.get("required") {
        let props = object.get("properties").and_then(Value::as_object);
        match required.as_array() {
            Some(names) => {
                for name in names {
                    match (name.as_str(), props) {
                        (Some(n), Some(p)) if p.contains_key(n) => {}
                        (Some(n), _) => {
                            errors.push(format!("{path}: required `{n}` has no property"))
                        }
                        (None, _) => {
                            errors.push(format!("{path}: required entries must be strings"))
                        }
                    }
                }
            }
            None => errors.push(format!("{path}: required must be an array")),
        }
    }
    if let Some(Value::Object(props)) = object.get("properties") {
        for (name, sub) in props {
            walk(root, sub, &format!("{path}/properties/{name}"), errors);
        }
    }
    if let Some(Value::Object(defs)) = object.get("$defs") {
        for (name, sub) in defs {
            walk(root, sub, &format!("{path}/$defs/{name}"), errors);
        }
    }
    for key in ["items", "additionalProperties", "not"] {
        if let Some(sub) = object.get(key) {
            walk(root, sub, &format!("{path}/{key}"), errors);
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(list) = object.get(key) {
            match list.as_array() {
                Some(items) if !items.is_empty() => {
                    for (i, sub) in items.iter().enumerate() {
                        walk(root, sub, &format!("{path}/{key}/{i}"), errors);
                    }
                }
                _ => errors.push(format!("{path}: {key} must be a non-empty array")),
            }
        }
    }
    if let Some(values) = object.get("enum")
        && values.as_array().is_none_or(Vec::is_empty)
    {
        errors.push(format!("{path}: enum must be a non-empty array"));
    }
}

fn resolve<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    root.pointer(pointer)
}

/// Validation errors of `instance` against `schema` (the schema document is
/// its own root for `$ref` resolution).
pub(crate) fn validate(schema: &Value, instance: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    check(schema, schema, instance, "$", &mut errors);
    errors
}

fn is_valid(root: &Value, schema: &Value, instance: &Value) -> bool {
    let mut errors = Vec::new();
    check(root, schema, instance, "$", &mut errors);
    errors.is_empty()
}

fn type_matches(t: &str, instance: &Value) -> bool {
    match t {
        "null" => instance.is_null(),
        "boolean" => instance.is_boolean(),
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "number" => instance.is_number(),
        "integer" => instance.is_i64() || instance.is_u64(),
        _ => false,
    }
}

fn check(root: &Value, schema: &Value, instance: &Value, path: &str, errors: &mut Vec<String>) {
    let object: &Map<String, Value> = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errors.push(format!("{path}: schema `false` rejects every value"));
            return;
        }
        Value::Object(object) => object,
        _ => {
            errors.push(format!("{path}: invalid schema"));
            return;
        }
    };
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        match resolve(root, reference) {
            Some(target) => check(root, target, instance, path, errors),
            None => errors.push(format!("{path}: unresolvable $ref {reference}")),
        }
    }
    match object.get("type") {
        Some(Value::String(t)) if !type_matches(t, instance) => {
            errors.push(format!("{path}: expected {t}, got {instance}"));
            return;
        }
        Some(Value::Array(ts))
            if !ts
                .iter()
                .filter_map(Value::as_str)
                .any(|t| type_matches(t, instance)) =>
        {
            errors.push(format!("{path}: expected one of {ts:?}, got {instance}"));
            return;
        }
        _ => {}
    }
    if let Some(Value::Array(values)) = object.get("enum")
        && !values.contains(instance)
    {
        errors.push(format!("{path}: {instance} is not one of {values:?}"));
    }
    if let Some(expected) = object.get("const")
        && expected != instance
    {
        errors.push(format!("{path}: expected const {expected}, got {instance}"));
    }
    if let Some(value) = instance.as_object() {
        let props = object.get("properties").and_then(Value::as_object);
        if let Some(Value::Array(required)) = object.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !value.contains_key(name) {
                    errors.push(format!("{path}: missing required property `{name}`"));
                }
            }
        }
        for (key, item) in value {
            let child = format!("{path}.{key}");
            match props.and_then(|p| p.get(key)) {
                Some(sub) => check(root, sub, item, &child, errors),
                None => match object.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        errors.push(format!("{path}: unexpected property `{key}`"));
                    }
                    Some(sub @ Value::Object(_)) => check(root, sub, item, &child, errors),
                    _ => {}
                },
            }
        }
    }
    if let Some(items) = instance.as_array() {
        if let Some(sub) = object.get("items") {
            for (i, item) in items.iter().enumerate() {
                check(root, sub, item, &format!("{path}[{i}]"), errors);
            }
        }
        if let Some(min) = object.get("minItems").and_then(Value::as_u64)
            && (items.len() as u64) < min
        {
            errors.push(format!("{path}: fewer than {min} items"));
        }
        if let Some(max) = object.get("maxItems").and_then(Value::as_u64)
            && (items.len() as u64) > max
        {
            errors.push(format!("{path}: more than {max} items"));
        }
    }
    if let Some(text) = instance.as_str() {
        let len = text.chars().count() as u64;
        if let Some(min) = object.get("minLength").and_then(Value::as_u64)
            && len < min
        {
            errors.push(format!("{path}: shorter than {min}"));
        }
        if let Some(max) = object.get("maxLength").and_then(Value::as_u64)
            && len > max
        {
            errors.push(format!("{path}: longer than {max}"));
        }
    }
    if let Some(number) = instance.as_f64() {
        if let Some(min) = object.get("minimum").and_then(Value::as_f64)
            && number < min
        {
            errors.push(format!("{path}: {number} < minimum {min}"));
        }
        if let Some(max) = object.get("maximum").and_then(Value::as_f64)
            && number > max
        {
            errors.push(format!("{path}: {number} > maximum {max}"));
        }
    }
    if let Some(Value::Array(all)) = object.get("allOf") {
        for sub in all {
            check(root, sub, instance, path, errors);
        }
    }
    if let Some(Value::Array(any)) = object.get("anyOf")
        && !any.iter().any(|sub| is_valid(root, sub, instance))
    {
        errors.push(format!("{path}: matches none of anyOf"));
    }
    if let Some(Value::Array(one)) = object.get("oneOf") {
        let matching = one
            .iter()
            .filter(|sub| is_valid(root, sub, instance))
            .count();
        if matching != 1 {
            errors.push(format!(
                "{path}: matches {matching} of oneOf (expected exactly 1)"
            ));
        }
    }
    if let Some(not) = object.get("not")
        && is_valid(root, not, instance)
    {
        errors.push(format!("{path}: matches `not`"));
    }
}

#[test]
fn checker_catches_violations() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "a": {"type": "integer", "minimum": 1},
            "b": {"$ref": "#/$defs/B"},
            "c": {"oneOf": [{"type": "string", "const": "x"}, {"type": "string", "const": "y"}]}
        },
        "required": ["a"],
        "additionalProperties": false,
        "$defs": {"B": {"type": "array", "items": {"type": "string"}, "maxItems": 1}}
    });
    assert!(check_well_formed(&schema).is_empty());
    assert!(validate(&schema, &serde_json::json!({"a": 1, "b": ["s"], "c": "x"})).is_empty());
    assert_eq!(validate(&schema, &serde_json::json!({"a": 0})).len(), 1);
    assert_eq!(
        validate(&schema, &serde_json::json!({"b": ["s", "t"]})).len(),
        2
    );
    assert_eq!(
        validate(&schema, &serde_json::json!({"a": 1, "z": 1})).len(),
        1
    );
    assert_eq!(
        validate(&schema, &serde_json::json!({"a": 1, "c": "q"})).len(),
        1
    );
    let broken = serde_json::json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/Missing"}}, "required": ["b"]});
    assert_eq!(check_well_formed(&broken).len(), 2);
}
