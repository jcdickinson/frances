//! `JsonRepair<T>` — absorb the qwen3-coder family quirk where array tool-call
//! arguments arrive double-encoded as JSON strings (e.g. `{"files": "[...]"}`
//! instead of `{"files": [...]}`).
//!
//! On strict deserialize success, we're a zero-cost passthrough. On a
//! `Category::Data` error we recursively unwrap any `Value::String` whose
//! contents parse to an array or object, then retry; if repair still fails,
//! the original strict error is surfaced.

use std::ops::Deref;

use serde::de::DeserializeOwned;
use serde_json::{Error, Value, error::Category};

#[derive(Debug, Clone, PartialEq)]
pub struct JsonRepair<T>(pub T);

impl<T> Deref for JsonRepair<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> JsonRepair<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: DeserializeOwned> JsonRepair<T> {
    /// Try strict `from_value`. On `Category::Data` errors (well-formed JSON
    /// but shape-mismatched against `T`'s schema), recursively unwrap any
    /// `Value::String` whose content parses to a JSON array or object, then
    /// retry. Returns the retry error on failure.
    pub fn from_value(v: Value) -> Result<Self, Error> {
        match serde_json::from_value::<T>(v.clone()) {
            Ok(t) => Ok(Self(t)),
            Err(strict_err) if strict_err.classify() == Category::Data => {
                let repaired = unwrap_stringified(v);
                serde_json::from_value::<T>(repaired).map(Self)
            }
            Err(other) => Err(other),
        }
    }
}

fn unwrap_stringified(v: Value) -> Value {
    match v {
        Value::String(s) => match serde_json::from_str::<Value>(&s) {
            Ok(inner @ (Value::Array(_) | Value::Object(_))) => unwrap_stringified(inner),
            _ => Value::String(s),
        },
        Value::Array(a) => Value::Array(a.into_iter().map(unwrap_stringified).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (k, unwrap_stringified(v)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize, Debug, PartialEq)]
    struct FileEntry {
        path: String,
        edits: Vec<Edit>,
    }

    #[derive(Deserialize, Debug, PartialEq)]
    struct Edit {
        anchor: String,
        text: String,
    }

    #[derive(Deserialize, Debug, PartialEq)]
    struct Input {
        files: Vec<FileEntry>,
    }

    #[test]
    fn strict_path_succeeds_for_well_formed_input() {
        let v = json!([1, 2, 3]);
        let parsed = JsonRepair::<Vec<i32>>::from_value(v).unwrap();
        assert_eq!(parsed.into_inner(), vec![1, 2, 3]);
    }

    #[test]
    fn unwraps_top_level_stringified_array() {
        let v = Value::String("[1, 2, 3]".to_string());
        let parsed = JsonRepair::<Vec<i32>>::from_value(v).unwrap();
        assert_eq!(parsed.into_inner(), vec![1, 2, 3]);
    }

    #[test]
    fn unwraps_nested_stringified_arrays() {
        // Both `files` and the inner `edits` are JSON-encoded strings — the
        // qwen3-coder quirk in full force. One repair pass fixes both.
        let v = json!({
            "files": "[{\"path\": \"src/x.rs\", \"edits\": \"[{\\\"anchor\\\": \\\"A§a\\\", \\\"text\\\": \\\"hi\\\"}]\"}]"
        });
        let parsed = JsonRepair::<Input>::from_value(v).unwrap();
        assert_eq!(
            parsed.into_inner(),
            Input {
                files: vec![FileEntry {
                    path: "src/x.rs".to_string(),
                    edits: vec![Edit {
                        anchor: "A§a".to_string(),
                        text: "hi".to_string()
                    }],
                }],
            }
        );
    }

    /// Mirrors a real failing tool call from `qwen/qwen3-coder-next`: `files`
    /// is a JSON-encoded string of an array of file entries; the inner `edits`
    /// is a normal inline array (not double-encoded). Multi-line `text`
    /// fields with literal newlines must be preserved verbatim.
    #[test]
    fn unwraps_observed_qwen_payload_shape() {
        let big = "[{\"path\":\"./CLAUDE.md\",\"edits\":[\
            {\"edit_type\":\"replace\",\"anchor\":\"From§# CLAUDE.md\",\
             \"end_anchor\":\"From§# CLAUDE.md\",\
             \"text\":\"# CLAUDE.md\\n## TEST FILE\\nLine three.\"},\
            {\"edit_type\":\"insert_after\",\"anchor\":\"Your§\",\
             \"text\":\"## Section\"}\
            ]}]";
        let v = json!({ "files": big });

        #[derive(Deserialize, Debug, PartialEq)]
        #[serde(tag = "edit_type", rename_all = "snake_case")]
        enum Edit2 {
            Replace {
                anchor: String,
                end_anchor: String,
                text: String,
            },
            InsertAfter {
                anchor: String,
                text: String,
            },
        }
        #[derive(Deserialize, Debug, PartialEq)]
        struct File2 {
            path: String,
            edits: Vec<Edit2>,
        }
        #[derive(Deserialize, Debug, PartialEq)]
        struct Input2 {
            files: Vec<File2>,
        }

        let parsed = JsonRepair::<Input2>::from_value(v).unwrap();
        let inner = parsed.into_inner();
        assert_eq!(inner.files.len(), 1);
        assert_eq!(inner.files[0].path, "./CLAUDE.md");
        assert_eq!(inner.files[0].edits.len(), 2);
        match &inner.files[0].edits[0] {
            Edit2::Replace { text, .. } => assert!(text.contains("\n")),
            _ => panic!("expected Replace"),
        }
    }

    #[test]
    fn unwraps_recursively() {
        // String of an object whose field is a string of an array.
        let inner_array = "[1, 2, 3]";
        let inner_obj = format!("{{\"nums\": {}}}", json!(inner_array));
        let v = Value::String(inner_obj);

        #[derive(Deserialize, Debug, PartialEq)]
        struct Wrap {
            nums: Vec<i32>,
        }

        let parsed = JsonRepair::<Wrap>::from_value(v).unwrap();
        assert_eq!(
            parsed.into_inner(),
            Wrap {
                nums: vec![1, 2, 3]
            }
        );
    }

    #[test]
    fn leaves_legitimate_strings_alone() {
        // Strict path succeeds — repair walk never fires, so the literal
        // JSON-looking string is preserved verbatim.
        let v = Value::String("[1, 2, 3]".to_string());
        let parsed = JsonRepair::<String>::from_value(v).unwrap();
        assert_eq!(parsed.into_inner(), "[1, 2, 3]");
    }

    #[test]
    fn unrepairable_input_surfaces_post_repair_error() {
        let v = json!({ "files": 42 });
        let err = JsonRepair::<Input>::from_value(v).unwrap_err();
        assert!(err.to_string().contains("expected"));
    }

    /// Mirrors a real failing call: model double-encoded `files` AND each
    /// inner object omitted the required `path` field. After repair, the
    /// retry should fail with `missing field 'path'`, not the stale
    /// "expected a sequence" message — the latter would mislead the model
    /// into thinking the encoding fix is what's needed.
    #[test]
    fn post_repair_error_names_the_remaining_problem() {
        let inner_only_edits = "[{\"edits\":[{\"anchor\":\"A§a\",\"text\":\"x\"}]}]";
        let v = json!({ "files": inner_only_edits });

        let err = JsonRepair::<Input>::from_value(v).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing field") && msg.contains("path"),
            "expected post-repair 'missing field path', got: {msg}"
        );
    }

    #[test]
    fn string_that_parses_to_scalar_is_not_unwrapped() {
        // `Value::String("42")` against `Vec<i32>`: strict fails (string not
        // array). The repair walk only unwraps to Array/Object, so leaves the
        // scalar-looking string alone. Repaired strict fails too; original
        // error returned.
        let v = Value::String("42".to_string());
        let err = JsonRepair::<Vec<i32>>::from_value(v).unwrap_err();
        assert_eq!(err.classify(), Category::Data);
    }
}
