use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use frances_config::{ConfigBinding, ConfigHandle, RequiredConfigBinding};
use serde::Serialize;
use serde_json::Value;

use crate::llm::config::ModelConfig;
use crate::llm::provider::{ErasedError, ProviderRequest};
use crate::llm::provider_cache::ProviderCache;

/// Convert an internal `anyhow::Error` to an [`ErasedError`] for the
/// [`crate::llm::provider::ErasedProvider`] boundary.
fn into_erased(e: anyhow::Error) -> ErasedError {
    e.into()
}

/// Log the underlying erased error and substitute a generic message before
/// it crosses back into anyhow-using daemon code.
fn log_and_generic(provider_id: &str, e: ErasedError) -> anyhow::Error {
    tracing::error!(provider = %provider_id, error = %e, "provider error");
    anyhow!("provider {} encountered an error", provider_id)
}

/// Chat-completions client. Vendor-neutral despite the name "Responses"
/// in `wire_api`: today the wire is OpenAI-style chat completions; if a
/// new wire is introduced it will live in a sibling module.
///
/// Model selection is name-driven: callers pass a fallback list of names
/// (e.g. `&["chat"]`) and the client looks each one up under
/// `models::<name>`. Bindings are cached on first use so repeat lookups
/// are lock-free reads. The fallback always terminates in `default_model`,
/// which is bound as required at startup.
#[derive(Clone)]
pub struct ChatClient {
    env: HashMap<OsString, OsString>,
    session_id: String,
    cache: Arc<ProviderCache>,
    config: ConfigHandle,
    default_model: RequiredConfigBinding<ModelConfig>,
    model_cache: Arc<Mutex<HashMap<String, ConfigBinding<ModelConfig>>>>,
}

impl ChatClient {
    pub fn new(
        env: HashMap<OsString, OsString>,
        session_id: String,
        cache: Arc<ProviderCache>,
        config: ConfigHandle,
        default_model: RequiredConfigBinding<ModelConfig>,
    ) -> Result<Self> {
        Ok(Self {
            env,
            session_id,
            cache,
            config,
            default_model,
            model_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn stream<F>(
        &self,
        names: &[&str],
        messages: &[Value],
        tools: &[ToolDef],
        tool_choice: Option<&ToolChoice>,
        mut on_chunk: F,
    ) -> Result<()>
    where
        F: FnMut(&Value) -> Result<()> + Send,
    {
        let model = self.resolve_model(names)?;
        let provider_id = model.model_provider.clone();
        let provider = self.cache.get(&provider_id).ok_or_else(|| {
            anyhow!(
                "model_providers.{} not available (no config or factory missing)",
                provider_id
            )
        })?;
        let req = ProviderRequest {
            session_id: &self.session_id,
            model: &model,
            messages,
            tools,
            tool_choice,
            env: &self.env,
        };
        let mut wrapped = |v: &Value| on_chunk(v).map_err(into_erased);
        provider
            .stream(req, &mut wrapped)
            .await
            .map_err(|e| log_and_generic(&provider_id, e))
    }

    /// One-shot wrapper around [`stream`](Self::stream): drives the SSE
    /// stream to completion and returns the full assistant text plus any
    /// finalized tool calls. Use this when the caller doesn't need to
    /// surface mid-stream deltas (e.g. the shell classifier).
    pub async fn complete(
        &self,
        names: &[&str],
        messages: &[Value],
        tools: &[ToolDef],
        tool_choice: Option<&ToolChoice>,
    ) -> Result<CompletionOutcome> {
        let model = self.resolve_model(names)?;
        let provider_id = model.model_provider.clone();
        let provider = self.cache.get(&provider_id).ok_or_else(|| {
            anyhow!(
                "model_providers.{} not available (no config or factory missing)",
                provider_id
            )
        })?;
        let req = ProviderRequest {
            session_id: &self.session_id,
            model: &model,
            messages,
            tools,
            tool_choice,
            env: &self.env,
        };
        provider
            .complete(req)
            .await
            .map_err(|e| log_and_generic(&provider_id, e))
    }

    /// Walks `names` in order, returning the first one whose `models::<name>`
    /// binding currently has a value. Falls through to `default_model`, which
    /// is a required (sticky-on-absence) binding and therefore always
    /// resolves. Bindings are looked up via [`Self::binding_for`] so the
    /// arc-swap snapshot is always consulted live and config updates are
    /// picked up without restart.
    fn resolve_model(&self, names: &[&str]) -> Result<ModelConfig> {
        for name in names {
            let binding = self.binding_for(name)?;
            if let Some(model) = binding.get() {
                return Ok((*model).clone());
            }
        }
        Ok((*self.default_model.get()).clone())
    }

    /// Returns the cached binding for `models::<name>`, creating one on first
    /// use. The cache stores the binding object, not its current value —
    /// `ConfigBinding::get()` is re-evaluated on every call so live config
    /// edits propagate. Names that are never present in config still get a
    /// binding kept around, which is fine: the set of distinct names asked
    /// for is bounded by the call sites in the binary.
    fn binding_for(&self, name: &str) -> Result<ConfigBinding<ModelConfig>> {
        let mut cache = self.model_cache.lock().expect("model_cache poisoned");
        if let Some(b) = cache.get(name) {
            return Ok(b.clone());
        }
        let b = self
            .config
            .bind::<ModelConfig>(["models", name])
            .with_context(|| format!("bind models::{name}"))?;
        cache.insert(name.to_string(), b.clone());
        Ok(b)
    }
}

/// Final result of [`ChatClient::complete`]. `text` is the concatenation of
/// all `content` deltas; `tool_calls` is the parsed tool-call list (ordered
/// by index, like `ToolCallAccumulator::finalize`).
#[derive(Debug, Clone)]
pub struct CompletionOutcome {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

pub fn chunk_text_deltas(chunk: &Value) -> impl Iterator<Item = &str> {
    chunk
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|choice| {
            choice
                .get("delta")
                .and_then(|delta| delta.get("content"))
                .and_then(Value::as_str)
        })
        .filter(|text| !text.is_empty())
}

/// One event extracted from a streamed `delta.tool_calls` entry. A single
/// wire chunk can carry both a `Start` (id + name appearing) and an
/// `Append` (initial argument fragment) — the parser splits them.
#[derive(Debug, Clone, Copy)]
pub struct ToolCallDelta<'a> {
    pub index: u32,
    pub event: ToolCallEvent<'a>,
}

#[derive(Debug, Clone, Copy)]
pub enum ToolCallEvent<'a> {
    /// First event for a given index: declares the tool's id and name.
    Start { id: &'a str, name: &'a str },
    /// Subsequent events: extend the JSON-string arguments buffer.
    Append(&'a str),
}

pub fn chunk_tool_call_deltas(chunk: &Value) -> Vec<ToolCallDelta<'_>> {
    let mut out = Vec::new();
    let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
        return out;
    };
    for choice in choices {
        let Some(tcs) = choice
            .get("delta")
            .and_then(|d| d.get("tool_calls"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for tc in tcs {
            let Some(index) = tc.get("index").and_then(Value::as_u64).map(|n| n as u32) else {
                continue;
            };
            let id = tc.get("id").and_then(Value::as_str);
            let function = tc.get("function");
            let name = function.and_then(|f| f.get("name")).and_then(Value::as_str);
            let fragment = function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str);

            if let (Some(id), Some(name)) = (id, name) {
                out.push(ToolCallDelta {
                    index,
                    event: ToolCallEvent::Start { id, name },
                });
            }
            if let Some(fragment) = fragment.filter(|f| !f.is_empty()) {
                out.push(ToolCallDelta {
                    index,
                    event: ToolCallEvent::Append(fragment),
                });
            }
        }
    }
    out
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "stream chunk inspector; exercised by tests, not yet by the runtime"
    )
)]
pub fn chunk_finish_reason(chunk: &Value) -> Option<&str> {
    chunk
        .get("choices")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|choice| choice.get("finish_reason").and_then(Value::as_str))
}

pub fn chunk_usage(chunk: &Value) -> Option<Usage> {
    let usage = chunk.get("usage")?;
    if usage.is_null() {
        return None;
    }
    Some(Usage {
        prompt_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        completion_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        // OpenAI-style chat completions (and OpenRouter's normalized shape)
        // nest cached prompt tokens under `prompt_tokens_details.cached_tokens`.
        cached_input_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    })
}

#[derive(Debug, Clone, Default, serde::Deserialize, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cached_input_tokens: u32,
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
/// whichever shape is appropriate.
#[derive(Clone, Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "tool_choice variants kept for caller flexibility; default `auto` is implicit when omitted"
    )
)]
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

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Builder for an in-flight tool call. Once a `Start` event creates the
/// entry, all fields are populated — there's no "id might still arrive"
/// state, because such a state would be a malformed stream.
struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub struct ToolCallAccumulator {
    in_progress: std::collections::BTreeMap<u32, ToolCallBuilder>,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, delta: ToolCallDelta<'_>) -> Result<()> {
        match delta.event {
            ToolCallEvent::Start { id, name } => {
                if self.in_progress.contains_key(&delta.index) {
                    return Err(anyhow!(
                        "tool call at index {} already started",
                        delta.index
                    ));
                }
                self.in_progress.insert(
                    delta.index,
                    ToolCallBuilder {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        arguments: String::new(),
                    },
                );
            }
            ToolCallEvent::Append(fragment) => {
                let builder = self.in_progress.get_mut(&delta.index).ok_or_else(|| {
                    anyhow!(
                        "argument fragment for unstarted tool call at index {}",
                        delta.index
                    )
                })?;
                builder.arguments.push_str(fragment);
            }
        }
        Ok(())
    }

    #[expect(
        dead_code,
        reason = "accumulator introspection; useful for debugging mid-stream state"
    )]
    pub fn is_empty(&self) -> bool {
        self.in_progress.is_empty()
    }

    /// Drains accumulated calls (sorted by index), parsing each call's JSON
    /// arguments into a `Value`. Errors if any arguments string is malformed.
    pub fn finalize(self) -> Result<Vec<ToolCall>> {
        self.in_progress
            .into_values()
            .map(|b| {
                let arguments: Value = serde_json::from_str(&b.arguments).with_context(|| {
                    format!("parse arguments for tool call {} ({})", b.id, b.name)
                })?;
                Ok(ToolCall {
                    id: b.id,
                    name: b.name,
                    arguments,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_delta_chunk_yields_text() {
        let chunk = json!({
            "choices": [{ "delta": { "content": "hello" } }]
        });
        let texts: Vec<&str> = chunk_text_deltas(&chunk).collect();
        assert_eq!(texts, vec!["hello"]);
    }

    #[test]
    fn text_delta_skips_empty() {
        let chunk = json!({
            "choices": [{ "delta": { "content": "" } }]
        });
        let texts: Vec<&str> = chunk_text_deltas(&chunk).collect();
        assert!(texts.is_empty());
    }

    #[test]
    fn first_chunk_yields_start_event_only() {
        let chunk = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_abc",
                        "type": "function",
                        "function": { "name": "edit", "arguments": "" }
                    }]
                }
            }]
        });
        let deltas = chunk_tool_call_deltas(&chunk);
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].index, 0);
        match deltas[0].event {
            ToolCallEvent::Start { id, name } => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "edit");
            }
            _ => panic!("expected Start event"),
        }
    }

    #[test]
    fn argument_fragment_yields_append_event() {
        let chunk = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "{\"files\":" }
                    }]
                }
            }]
        });
        let deltas = chunk_tool_call_deltas(&chunk);
        assert_eq!(deltas.len(), 1);
        match deltas[0].event {
            ToolCallEvent::Append(frag) => assert_eq!(frag, "{\"files\":"),
            _ => panic!("expected Append event"),
        }
    }

    #[test]
    fn chunk_with_start_and_nonempty_args_yields_two_events() {
        let chunk = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "x",
                        "function": { "name": "edit", "arguments": "{}" }
                    }]
                }
            }]
        });
        let deltas = chunk_tool_call_deltas(&chunk);
        assert_eq!(deltas.len(), 2);
        assert!(matches!(deltas[0].event, ToolCallEvent::Start { .. }));
        assert!(matches!(deltas[1].event, ToolCallEvent::Append("{}")));
    }

    #[test]
    fn parallel_calls_yield_distinct_indices() {
        let chunk = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "a", "function": {"name": "file_read"}},
                        {"index": 1, "id": "b", "function": {"name": "edit"}}
                    ]
                }
            }]
        });
        let deltas = chunk_tool_call_deltas(&chunk);
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].index, 0);
        assert_eq!(deltas[1].index, 1);
    }

    #[test]
    fn finish_reason_present_and_absent() {
        let stop = json!({"choices": [{"finish_reason": "stop"}]});
        assert_eq!(chunk_finish_reason(&stop), Some("stop"));
        let tool = json!({"choices": [{"finish_reason": "tool_calls"}]});
        assert_eq!(chunk_finish_reason(&tool), Some("tool_calls"));
        let none = json!({"choices": [{"delta": {"content": "x"}}]});
        assert_eq!(chunk_finish_reason(&none), None);
    }

    #[test]
    fn accumulator_end_to_end_single_call() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Start {
                id: "call_1",
                name: "edit",
            },
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Append("{\"files\":"),
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Append("[]}"),
        })
        .unwrap();
        let calls = acc.finalize().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "edit");
        assert_eq!(calls[0].arguments, json!({"files": []}));
    }

    #[test]
    fn accumulator_two_parallel_calls_sorted_by_index() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(ToolCallDelta {
            index: 1,
            event: ToolCallEvent::Start {
                id: "b",
                name: "edit",
            },
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Start {
                id: "a",
                name: "file_read",
            },
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Append("{}"),
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 1,
            event: ToolCallEvent::Append("{\"files\":[]}"),
        })
        .unwrap();
        let calls = acc.finalize().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "a");
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].id, "b");
        assert_eq!(calls[1].name, "edit");
    }

    #[test]
    fn accumulator_rejects_append_before_start() {
        let mut acc = ToolCallAccumulator::new();
        let err = acc
            .push(ToolCallDelta {
                index: 0,
                event: ToolCallEvent::Append("{}"),
            })
            .unwrap_err();
        assert!(err.to_string().contains("unstarted"));
    }

    #[test]
    fn accumulator_rejects_double_start() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Start {
                id: "x",
                name: "edit",
            },
        })
        .unwrap();
        let err = acc
            .push(ToolCallDelta {
                index: 0,
                event: ToolCallEvent::Start {
                    id: "y",
                    name: "edit",
                },
            })
            .unwrap_err();
        assert!(err.to_string().contains("already started"));
    }

    #[test]
    fn accumulator_finalize_errors_on_malformed_arguments() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Start {
                id: "x",
                name: "edit",
            },
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Append("not json"),
        })
        .unwrap();
        let err = acc.finalize().unwrap_err();
        assert!(err.to_string().contains("parse arguments"));
    }

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
