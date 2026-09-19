//! A deliberately small, zero-dependency JSON-Schema checker.
//!
//! Full JSON Schema would need a new crate (forbidden by the offline / no-new
//! -deps rule), so this validates the subset tool servers actually advertise:
//! `type` (including type unions), `required`, `properties`, `enum`, and one
//! level of `items`. Anything it does not understand is treated as satisfied,
//! so a permissive or unusual schema can never block a legitimate tool call.

use serde_json::Value;

/// Validate `input` (a tool's raw JSON arguments) against `schema`. `Ok(())`
/// means "acceptable" (including when the schema is empty or unknown); `Err`
/// carries a short human-readable reason for a definite mismatch.
pub fn validate_tool_input(schema: &Value, input: &str) -> Result<(), String> {
    // No usable schema (null, an empty object, or a boolean form): the contract
    // asserts nothing, so advertise-and-run and do not even require the input
    // to be parseable JSON. Only a keyword-bearing schema can reject a call.
    if is_permissive(schema) {
        return Ok(());
    }
    let parsed: Value = serde_json::from_str(input)
        .map_err(|error| format!("tool input is not valid JSON: {error}"))?;
    let mut path = String::from("$");
    check(schema, &parsed, &mut path)
}

fn is_permissive(schema: &Value) -> bool {
    match schema {
        Value::Null | Value::Bool(_) => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// Best-effort schema *normalization* run on a tool's advertised `input_schema`
/// before it reaches the provider. Some MCP servers emit shapes strict providers
/// reject or waste tokens on: an `object` with no `properties`, an `array` with
/// no `items`, or a single-entry `type` union. This tidies the definition only;
/// it never inspects or gates a call. Depth-capped so a recursive schema cannot
/// loop. Mirrors the compaction the reference Rust CLI applies to tool schemas.
pub fn normalize_tool_schema(schema: &mut Value) {
    normalize_value(schema, 0);
}

fn normalize_value(node: &mut Value, depth: usize) {
    const MAX_DEPTH: usize = 32;
    if depth >= MAX_DEPTH {
        return;
    }
    let Some(map) = node.as_object_mut() else {
        return;
    };
    // Collapse a one-entry `type` array (`["string"]`) to the bare string.
    if let Some(types) = map.get("type").and_then(Value::as_array).cloned() {
        if types.len() == 1 {
            if let Some(single) = types.into_iter().next() {
                map.insert(String::from("type"), single);
            }
        }
    }
    let wants_object = type_kinds(map).contains(&"object");
    let wants_array = type_kinds(map).contains(&"array");
    if wants_object && !map.contains_key("properties") {
        map.insert(
            String::from("properties"),
            Value::Object(serde_json::Map::new()),
        );
    }
    if wants_array && !map.contains_key("items") {
        map.insert(String::from("items"), Value::Object(serde_json::Map::new()));
    }
    if let Some(properties) = map.get_mut("properties").and_then(Value::as_object_mut) {
        for sub in properties.values_mut() {
            normalize_value(sub, depth + 1);
        }
    }
    if let Some(items) = map.get_mut("items") {
        normalize_value(items, depth + 1);
    }
}

fn type_kinds(map: &serde_json::Map<String, Value>) -> Vec<&str> {
    match map.get("type") {
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn check(schema: &Value, value: &Value, path: &mut String) -> Result<(), String> {
    let Some(object) = schema.as_object() else {
        // Boolean/legacy schema form (`true`, a bare type string): permissive.
        return Ok(());
    };
    if object.is_empty() {
        return Ok(());
    }

    if let Some(types) = object.get("type") {
        check_type(types, value, path)?;
    }
    if let Some(allowed) = object.get("enum").and_then(Value::as_array) {
        if !allowed.iter().any(|option| option == value) {
            return Err(format!("{path} is not one of the allowed values"));
        }
    }
    if value.is_object() {
        check_object(object, value, path)?;
    }
    if value.is_array() {
        check_array(object, value, path)?;
    }
    Ok(())
}

fn check_type(types: &Value, value: &Value, path: &str) -> Result<(), String> {
    let matches = |ty: &str| match ty {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => match value {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        "number" => value.is_number(),
        "null" => value.is_null(),
        // Unknown type keyword: do not reject on something we cannot interpret.
        _ => true,
    };
    let ok = match types {
        Value::String(ty) => matches(ty),
        Value::Array(list) => list.iter().filter_map(Value::as_str).any(&matches),
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(format!("{path} has the wrong type"))
    }
}

fn check_object(
    schema: &serde_json::Map<String, Value>,
    value: &Value,
    path: &mut String,
) -> Result<(), String> {
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if value.get(key).is_none() {
                return Err(format!("{path} is missing required field '{key}'"));
            }
        }
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (key, sub) in properties {
            if let Some(child) = value.get(key) {
                let saved = path.clone();
                path.push('.');
                path.push_str(key);
                let result = check(sub, child, path);
                *path = saved;
                result?;
            }
        }
    }
    Ok(())
}

fn check_array(
    schema: &serde_json::Map<String, Value>,
    value: &Value,
    path: &mut String,
) -> Result<(), String> {
    if let Some(items) = schema.get("items") {
        for element in value.as_array().into_iter().flatten() {
            let saved = path.clone();
            path.push_str("[]");
            let result = check(items, element, path);
            *path = saved;
            result?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{normalize_tool_schema, validate_tool_input};
    use serde_json::json;

    #[test]
    fn normalizes_loose_provider_unfriendly_shapes() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "mode": { "type": ["string"] },
                "tags": { "type": "array" },
                "meta": { "type": "object" }
            }
        });
        normalize_tool_schema(&mut schema);
        // Single-entry type union collapses to a bare string.
        assert_eq!(schema["properties"]["mode"]["type"], json!("string"));
        // Arrays gain an `items`, objects gain `properties`.
        assert!(schema["properties"]["tags"]["items"].is_object());
        assert!(schema["properties"]["meta"]["properties"].is_object());
    }

    #[test]
    fn normalization_preserves_declared_children_and_is_idempotent() {
        let mut schema = json!({
            "type": "object",
            "properties": { "a": { "type": "string", "enum": ["x"] } }
        });
        normalize_tool_schema(&mut schema);
        let once = schema.clone();
        normalize_tool_schema(&mut schema);
        assert_eq!(schema, once);
        assert_eq!(schema["properties"]["a"]["enum"], json!(["x"]));
    }

    #[test]
    fn empty_or_null_schema_never_blocks() {
        assert!(validate_tool_input(&json!({}), "not json at all").is_ok());
        assert!(validate_tool_input(&json!(null), "{}").is_ok());
    }

    #[test]
    fn requires_object_and_present_keys() {
        let schema = json!({
            "type": "object",
            "required": ["command"],
            "properties": { "command": { "type": "string" } }
        });
        assert!(validate_tool_input(&schema, r#"{"command":"ls"}"#).is_ok());
        // Missing a required key.
        assert!(validate_tool_input(&schema, "{}").is_err());
        // Wrong type for a known property.
        assert!(validate_tool_input(&schema, r#"{"command":5}"#).is_err());
    }

    #[test]
    fn invalid_json_input_is_rejected_when_schema_is_constraining() {
        let schema = json!({ "type": "object", "required": ["x"] });
        assert!(validate_tool_input(&schema, "{not json").is_err());
    }

    #[test]
    fn supports_enum_items_and_type_unions() {
        let schema = json!({
            "type": "object",
            "properties": {
                "mode": { "enum": ["fast", "slow"] },
                "tags": { "type": "array", "items": { "type": "string" } },
                "maybe": { "type": ["string", "null"] }
            }
        });
        assert!(
            validate_tool_input(&schema, r#"{"mode":"fast","tags":["a","b"],"maybe":null}"#)
                .is_ok()
        );
        assert!(validate_tool_input(&schema, r#"{"mode":"nope"}"#).is_err());
        assert!(validate_tool_input(&schema, r#"{"tags":["a",3]}"#).is_err());
        assert!(validate_tool_input(&schema, r#"{"maybe":"text"}"#).is_ok());
    }

    #[test]
    fn unknown_keywords_are_ignored() {
        // A schema using features we do not model still accepts valid shapes.
        let schema = json!({ "type": "object", "minProperties": 5, "format": "email" });
        assert!(validate_tool_input(&schema, r#"{"a":1}"#).is_ok());
    }
}
