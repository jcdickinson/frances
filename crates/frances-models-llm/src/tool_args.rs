//! Validating LLM tool-call arguments against a tool's declared JSON
//! schema, and deciding when a schema qualifies for OpenAI strict mode.
//!
//! The model is asked to call a tool whose `parameters` is a JSON schema;
//! it returns `arguments` (arbitrary JSON). [`validate`] checks the latter
//! against the former so a malformed call can be turned into an error tool
//! result (the model then self-corrects on the next round).
//!
//! Note: the qwen3-coder family double-encodes non-scalar args as JSON
//! strings, which `validate` will reject until the deterministic repair
//! lands — see `docs/todo/qwen-tool-arg-repair.md`.

use serde_json::Value;

/// Validate `args` against a tool's `parameters` JSON schema. Returns a
/// concise, model-facing error string on mismatch.
///
/// A schema that itself fails to compile is treated as "can't validate"
/// (returns `Ok`) — that's a tool-author bug, not the model's fault, and
/// we'd rather not block the call on it.
pub fn validate(args: &Value, schema: &Value) -> Result<(), String> {
    let Ok(validator) = jsonschema::validator_for(schema) else {
        return Ok(());
    };
    match validator.validate(args) {
        Ok(()) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// Whether `schema` satisfies OpenAI strict structured-outputs rules, so we
/// can set `strict: true` "when possible". Strict mode rejects extensible
/// schemas: every object must set `additionalProperties: false` and list
/// **every** property in `required` (recursively, through nested objects
/// and array `items`).
pub fn is_strict_compatible(schema: &Value) -> bool {
    let Value::Object(map) = schema else {
        return true;
    };
    let ty = map.get("type").and_then(Value::as_str);
    if ty == Some("object") || map.contains_key("properties") {
        if map.get("additionalProperties") != Some(&Value::Bool(false)) {
            return false;
        }
        let Some(props) = map.get("properties").and_then(Value::as_object) else {
            return true;
        };
        let required: std::collections::HashSet<&str> = map
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        props
            .iter()
            .all(|(k, sub)| required.contains(k.as_str()) && is_strict_compatible(sub))
    } else if ty == Some("array") {
        map.get("items").is_none_or(is_strict_compatible)
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decide_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "verdict": { "type": "string", "enum": ["approve", "decline"] },
                "reason": { "type": "string" },
            },
            "required": ["verdict", "reason"],
        })
    }

    #[test]
    fn valid_args_pass() {
        let schema = decide_schema();
        assert!(validate(&json!({ "verdict": "approve", "reason": "ok" }), &schema).is_ok());
    }

    #[test]
    fn wrong_enum_fails() {
        let schema = decide_schema();
        let err = validate(&json!({ "verdict": "maybe", "reason": "x" }), &schema)
            .expect_err("verdict outside enum should fail");
        assert!(!err.is_empty());
    }

    #[test]
    fn missing_required_fails() {
        let schema = decide_schema();
        assert!(validate(&json!({ "reason": "x" }), &schema).is_err());
    }

    #[test]
    fn extra_property_fails_under_additional_properties_false() {
        let schema = decide_schema();
        assert!(
            validate(
                &json!({ "verdict": "approve", "reason": "x", "extra": 1 }),
                &schema
            )
            .is_err()
        );
    }

    #[test]
    fn uncompilable_schema_skips() {
        // A nonsense schema can't compile → we don't block the call.
        assert!(validate(&json!({ "type": "definitely-not-a-type" }), &json!(42)).is_ok());
    }

    #[test]
    fn strict_compat_detection() {
        assert!(is_strict_compatible(&decide_schema()));
        // Missing additionalProperties: false.
        assert!(!is_strict_compatible(&json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "required": ["a"],
        })));
        // Optional property (not in required).
        assert!(!is_strict_compatible(&json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "a": { "type": "string" }, "b": { "type": "string" } },
            "required": ["a"],
        })));
    }
}
