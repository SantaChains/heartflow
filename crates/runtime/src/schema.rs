//! Tool-argument JSON-Schema validation and provider-facing schema cleanup.
//!
//! Validation uses the `jsonschema` crate (full draft support: `minLength`,
//! `pattern`, `additionalProperties`, `$ref`, …). Two contracts keep a
//! validation upgrade from ever breaking legitimate calls: a permissive or
//! malformed schema (null/bool/empty, or one that fails to compile) is never
//! allowed to block a call — only a definite mismatch against a well-formed
//! schema rejects input.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

use serde_json::Value;

/// Process-wide cache of compiled validators, keyed by the schema's canonical
/// JSON text. A tool's schema never changes within a session, so each distinct
/// schema compiles once instead of on every tool call (compilation is the
/// expensive part; `Validator` is `Send + Sync` and cheap to clone).
fn validator_cache() -> &'static Mutex<HashMap<String, jsonschema::Validator>> {
    static CACHE: OnceLock<Mutex<HashMap<String, jsonschema::Validator>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Compile (or fetch from cache) the validator for `schema`. `None` means the
/// schema could not be canonicalized or compiled — the caller then applies the
/// permissive contract and lets the call through. A poisoned cache lock is
/// *not* one of those reasons: the cache holds nothing but compiled validators,
/// so the guard is recovered rather than dropped, because dropping it would
/// silently turn off tool-argument validation for the rest of the process.
fn compiled_validator(schema: &Value) -> Option<jsonschema::Validator> {
    let key = serde_json::to_string(schema).ok()?;
    {
        let cache = validator_cache()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(validator) = cache.get(&key) {
            return Some(validator.clone());
        }
    }
    // Compile outside the lock: compiling a schema is the expensive part, and a
    // slow compile on one thread must not stall cache lookups on another. Two
    // threads racing on the same uncached schema merely compile it twice — the
    // result is deterministic and the insert is idempotent, so that is benign.
    let validator = jsonschema::validator_for(schema).ok()?;
    validator_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, validator.clone());
    Some(validator)
}

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
    match compiled_validator(schema) {
        Some(validator) => validator.validate(&parsed).map_err(|error| {
            let reason = error.to_string();
            // ValidationError renders multi-line; keep the transcript single-line.
            let first = reason.lines().next().unwrap_or(reason.as_str()).trim();
            format!("tool input does not match the advertised schema: {first}")
        }),
        // Uncompilable schema (unsupported draft content, unresolvable $ref):
        // never block what we cannot interpret.
        None => Ok(()),
    }
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
        // Vendor keywords the spec does not define must never turn into a
        // rejection; `format` stays an annotation unless explicitly enforced.
        let schema = json!({ "type": "object", "x-heartflow-hint": 5, "format": "email" });
        assert!(validate_tool_input(&schema, r#"{"a":1}"#).is_ok());
    }

    #[test]
    fn uncompilable_schema_never_blocks() {
        // A malformed keyword value makes the schema uncompilable; such a
        // schema must not reject a well-formed call.
        let schema = json!({ "type": "object", "properties": { "a": { "minLength": "oops" } } });
        assert!(validate_tool_input(&schema, r#"{"a":"text"}"#).is_ok());
    }

    #[test]
    fn full_draft_keywords_are_enforced() {
        // The upgrade delta over the old bounded checker: constraints beyond
        // type/required/properties/enum/items now genuinely gate bad input.
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "minLength": 3, "pattern": "^[a-z]+$" },
                "count": { "type": "integer", "minimum": 1, "maximum": 10 }
            },
            "additionalProperties": false
        });
        assert!(validate_tool_input(&schema, r#"{"name":"abc","count":5}"#).is_ok());
        assert!(validate_tool_input(&schema, r#"{"name":"a","count":5}"#).is_err());
        assert!(validate_tool_input(&schema, r#"{"name":"AB","count":5}"#).is_err());
        assert!(validate_tool_input(&schema, r#"{"count":50}"#).is_err());
        assert!(validate_tool_input(&schema, r#"{"name":"abc","count":5,"extra":1}"#).is_err());
    }
}
