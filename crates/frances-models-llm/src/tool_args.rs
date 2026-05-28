//! Validating LLM tool-call arguments against a tool's declared JSON
//! schema, and deciding when a schema qualifies for OpenAI strict mode.
//!
//! The model is asked to call a tool whose `parameters` is a JSON schema;
//! it returns `arguments` (arbitrary JSON). [`validate`] checks the latter
//! against the former so a malformed call can be turned into an error tool
//! result (the model then self-corrects on the next round).
//!
//! The qwen3-coder family ships non-scalar tool-call args as JSON-encoded
//! strings — [`repair_qwen_quirks`] swaps those back to their structured
//! form before validation runs.

use serde_json::Value;

use crate::{ToolCall, ToolCallError, ToolDef};

/// Validate each call's arguments against the called tool's declared schema,
/// flagging mismatches in-place via [`ToolCall::error`]. A call to a tool not
/// present in `tools` (no schema to check) is left untouched — the dispatch
/// layer reports "tool not found" for it as before.
pub fn annotate(calls: &mut [ToolCall], tools: &[ToolDef]) {
    for call in calls {
        let Some(schema) = tools
            .iter()
            .find_map(|ToolDef::Function(f)| (f.name == call.name).then_some(&f.parameters))
        else {
            continue;
        };
        if let Err(message) = validate(&call.arguments, schema) {
            call.error = Some(ToolCallError {
                expected_schema: schema.clone(),
                message,
            });
        }
    }
}

/// Repair the qwen3-coder family's "non-scalar args as JSON strings" quirk
/// in-place on one tool call: for each top-level argument whose declared
/// schema type is `object` or `array` but whose value arrived as a
/// `Value::String`, attempt `serde_json::from_str` and substitute the
/// parsed value. Schema-driven so well-behaved providers are untouched
/// (only object/array slots are considered); shallow because the quirk
/// only ever stringifies the top-level parameter, never nested values.
pub fn repair_qwen_quirks(call: &mut ToolCall, tools: &[ToolDef]) {
    let Some(schema) = tools
        .iter()
        .find_map(|ToolDef::Function(f)| (f.name == call.name).then_some(&f.parameters))
    else {
        return;
    };
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let Some(args) = call.arguments.as_object_mut() else {
        return;
    };
    for (key, prop_schema) in props {
        let Some(slot) = args.get_mut(key) else {
            continue;
        };
        let Value::String(raw) = slot else { continue };
        let expected = prop_schema.get("type").and_then(Value::as_str);
        if !matches!(expected, Some("object" | "array")) {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
            *slot = parsed;
        }
    }
}

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
    fn annotate_flags_only_bad_calls() {
        use crate::{ToolCall, ToolDef, ToolFunction};
        let tools = vec![ToolDef::Function(ToolFunction {
            name: "decide".into(),
            description: String::new(),
            parameters: decide_schema(),
        })];
        let mk = |name: &str, args: Value| ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: args,
            error: None,
        };
        let mut calls = vec![
            mk("decide", json!({ "verdict": "approve", "reason": "ok" })),
            mk("decide", json!({ "verdict": "maybe" })),
            mk("unknown", json!({ "anything": true })),
        ];
        annotate(&mut calls, &tools);
        assert!(calls[0].error.is_none(), "valid call stays clean");
        let err = calls[1].error.as_ref().expect("bad args flagged");
        assert!(!err.message.is_empty());
        assert_eq!(err.expected_schema, decide_schema());
        assert!(calls[2].error.is_none(), "unknown tool isn't ours to flag");
    }

    fn report_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "title": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "owner": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "name": { "type": "string" },
                    },
                    "required": ["name"],
                },
            },
            "required": ["title", "tags", "owner"],
        })
    }

    fn report_tools() -> Vec<ToolDef> {
        use crate::{ToolDef, ToolFunction};
        vec![ToolDef::Function(ToolFunction {
            name: "submit_report".into(),
            description: String::new(),
            parameters: report_schema(),
        })]
    }

    fn mk_call(args: Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "submit_report".into(),
            arguments: args,
            error: None,
        }
    }

    #[test]
    fn repair_unstringifies_array_and_object_args() {
        let tools = report_tools();
        let mut call = mk_call(json!({
            "title": "Q3",
            "tags": "[\"a\",\"b\"]",
            "owner": "{\"name\":\"Alice\"}",
        }));
        repair_qwen_quirks(&mut call, &tools);
        assert_eq!(
            call.arguments,
            json!({ "title": "Q3", "tags": ["a", "b"], "owner": { "name": "Alice" } })
        );
    }

    #[test]
    fn repair_leaves_scalar_args_alone_even_when_quoted_looking() {
        // String-typed `title` stays a string even if its content
        // happens to be a JSON literal.
        let tools = report_tools();
        let mut call = mk_call(json!({
            "title": "[\"not\",\"an\",\"array\"]",
            "tags": ["a"],
            "owner": { "name": "Alice" },
        }));
        repair_qwen_quirks(&mut call, &tools);
        assert_eq!(call.arguments["title"], json!("[\"not\",\"an\",\"array\"]"));
    }

    #[test]
    fn repair_leaves_malformed_strings_alone() {
        let tools = report_tools();
        let mut call = mk_call(json!({
            "title": "Q3",
            "tags": "not-json",
            "owner": { "name": "Alice" },
        }));
        repair_qwen_quirks(&mut call, &tools);
        assert_eq!(call.arguments["tags"], json!("not-json"));
    }

    #[test]
    fn repair_noop_when_already_structured() {
        let tools = report_tools();
        let before = json!({
            "title": "Q3",
            "tags": ["a", "b"],
            "owner": { "name": "Alice" },
        });
        let mut call = mk_call(before.clone());
        repair_qwen_quirks(&mut call, &tools);
        assert_eq!(call.arguments, before);
    }

    #[test]
    fn repair_noop_when_tool_unknown() {
        let tools = report_tools();
        let before = json!({ "tags": "[\"a\"]" });
        let mut call = ToolCall {
            id: "c".into(),
            name: "other_tool".into(),
            arguments: before.clone(),
            error: None,
        };
        repair_qwen_quirks(&mut call, &tools);
        assert_eq!(call.arguments, before);
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
