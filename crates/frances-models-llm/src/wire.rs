//! Value types exchanged with provider implementations: history inputs,
//! stream events, completion outcomes, tool definitions.
//!
//! `Provider` trait + transport machinery live in `frances-llm`; this
//! file is purely data shared between the manager and its providers.

use serde::Serialize;
use serde_json::Value;

/// Primitive content the provider may need to forge into wire shape — both
/// inline during `stream` (for the just-arrived turn delta) and in batch
/// during a swap-time `forge_history` call (rebuilding the cache from
/// every primitive row in the conversation).
#[derive(Debug, Clone)]
pub enum HistoryInput<'a> {
    System {
        text: &'a str,
    },
    User {
        text: &'a str,
    },
    Assistant {
        text: &'a str,
    },
    ToolCall {
        id: &'a str,
        name: &'a str,
        arguments: &'a Value,
    },
    ToolResult {
        call_id: &'a str,
        content: &'a str,
        is_error: bool,
    },
}

/// Streaming events emitted by provider `stream` implementations.
///
/// `History` events carry the wire-shape JSON the provider would put back
/// into the next request. The provider emits one (or more) per
/// `req.new_inputs` entry it forges, plus one (or more) for the assistant
/// turn it just produced. The runtime caches them verbatim.
///
/// `ToolCall` events are emitted once each as a fully-parsed [`ToolCall`].
/// OpenAI-shaped wires can't reliably mark per-call completion mid-stream,
/// so the implementation fires these at end-of-stream just before the
/// stream returns. The same calls are also returned in
/// [`CompletionOutcome::tool_calls`].
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A fragment of assistant text. Concatenate to obtain the running text.
    TextDelta(String),
    /// A completed tool call.
    ToolCall(ToolCall),
    /// A wire-shape JSON to be persisted for use as future history.
    History(Value),
    /// Final-frame token accounting. May be emitted once at the end of the
    /// stream; not all wires populate it.
    Usage(Usage),
}

/// Boxed error type used at the type-erased provider boundary. Any concrete
/// provider error that converts in both directions with this box (e.g. a
/// thiserror enum that derives `Error`, plus a manual `From<ErasedError>`)
/// can be wrapped.
pub type ErasedError = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type ErasedResult<T> = std::result::Result<T, ErasedError>;

/// Signal value used at the type-erased provider boundary to abort a
/// stream when the caller-provided `on_event` callback returned an error.
/// The erased layer swallows the synthesised error and surfaces the
/// original caller error.
#[derive(Debug)]
pub struct ChunkAbort;
impl std::fmt::Display for ChunkAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("on_event callback aborted")
    }
}
impl std::error::Error for ChunkAbort {}

/// Final result of a provider stream call. `text` is the concatenation of
/// all `TextDelta` events; `tool_calls` is the parsed tool-call list, ordered
/// as the model emitted them. Each call carries an optional
/// [`ToolCallError`](ToolCall::error): the chat layer validates arguments
/// against the called tool's schema (`tool_args::annotate`) and flags the
/// ones that don't, so dispatch can hand the model a corrective error result.
#[derive(Debug, Clone)]
pub struct CompletionOutcome {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

/// Wire shape: `{"type": "function", "function": {...}}`. Adjacently tagged
/// so the variant name becomes the `type` value and the inner struct sits
/// under the `function` key. New tool kinds (if OpenAI ever ships them)
/// would be added as additional variants here.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", content = "function", rename_all = "snake_case")]
pub enum ToolDef {
    Function(ToolFunction),
}

#[derive(Serialize, Clone, Debug)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Per OpenAI's spec, `tool_choice` is either a string mode (`"auto"`,
/// `"none"`, `"required"`) or an object pinning a specific function:
/// `{"type":"function","function":{"name":"..."}}`. This enum serializes to
/// whichever shape is appropriate. Variants are kept for caller flexibility;
/// callers default to `auto` by omitting `tool_choice` from the request.
#[derive(Clone, Debug)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Function(String),
}

impl Serialize for ToolChoice {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Auto => ser.serialize_str("auto"),
            Self::None => ser.serialize_str("none"),
            Self::Required => ser.serialize_str("required"),
            Self::Function(name) => {
                let mut map = ser.serialize_map(Some(2))?;
                map.serialize_entry("type", "function")?;
                map.serialize_entry("function", &serde_json::json!({ "name": name }))?;
                map.end()
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON-shaped args the model supplied for this call.
    pub arguments: Value,
    /// `Some` when the chat layer validated `arguments` against the called
    /// tool's declared schema and they didn't match. The call was still
    /// emitted by the model (so it stays in `tool_calls` and gets persisted);
    /// dispatch turns it into an error tool result the model can correct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolCallError>,
}

/// Why a [`ToolCall`]'s arguments failed schema validation, plus the schema
/// they were expected to satisfy. Enough for the caller to build a corrective
/// error result the model can self-correct against.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallError {
    /// The JSON schema the arguments were checked against. Encoded as a string
    /// for the same bincode reason as [`ToolCall::arguments`].
    #[serde(with = "json_value_as_string")]
    pub expected_schema: Value,
    pub message: String,
}

mod json_value_as_string {
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_json::Value;

    pub fn serialize<S: Serializer>(value: &Value, serializer: S) -> Result<S::Ok, S::Error> {
        let s = serde_json::to_string(value).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
        let s = String::deserialize(deserializer)?;
        serde_json::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Token-usage report. Universal shape; `cached_input_tokens` mirrors
/// OpenAI's `prompt_tokens_details.cached_tokens` for the wires that
/// surface it.
#[derive(Debug, Clone, Default, serde::Deserialize, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cached_input_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tooldef_serializes_to_openai_shape() {
        let td = ToolDef::Function(ToolFunction {
            name: "edit".into(),
            description: "Apply a patch".into(),
            parameters: json!({"type": "object"}),
        });
        let serialized = serde_json::to_value(&td).unwrap();
        assert_eq!(serialized["type"], "function");
        assert_eq!(serialized["function"]["name"], "edit");
        assert_eq!(serialized["function"]["description"], "Apply a patch");
    }

    #[test]
    fn toolchoice_modes_serialize_to_strings() {
        assert_eq!(serde_json::to_value(&ToolChoice::Auto).unwrap(), "auto");
        assert_eq!(serde_json::to_value(&ToolChoice::None).unwrap(), "none");
        assert_eq!(
            serde_json::to_value(&ToolChoice::Required).unwrap(),
            "required"
        );
    }

    #[test]
    fn toolchoice_function_serializes_to_object() {
        let v = serde_json::to_value(ToolChoice::Function("edit".into())).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "edit");
    }
}
