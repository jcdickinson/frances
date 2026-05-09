use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use frances_config::{ConfigBinding, EnvLookup};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use tracing::{debug, trace, warn};
use url::Url;

use crate::llm::config::{
    AuthMethod, ModelConfig, ModelsConfig, ProviderConfig, ResponsesModelExtras,
};

/// Chat-completions client. Vendor-neutral despite the name "Responses"
/// in `wire_api`: today the wire is OpenAI-style chat completions; if a
/// new wire is introduced it will live in a sibling module.
#[derive(Clone)]
pub struct ChatClient {
    http: reqwest::Client,
    env: Vec<(OsString, OsString)>,
    providers: ConfigBinding<HashMap<String, ProviderConfig>>,
    models: ConfigBinding<ModelsConfig>,
    extras: ConfigBinding<HashMap<String, ResponsesModelExtras>>,
}

impl ChatClient {
    pub fn new(
        env: Vec<(OsString, OsString)>,
        providers: ConfigBinding<HashMap<String, ProviderConfig>>,
        models: ConfigBinding<ModelsConfig>,
        extras: ConfigBinding<HashMap<String, ResponsesModelExtras>>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            http,
            env,
            providers,
            models,
            extras,
        })
    }

    pub async fn stream<F>(
        &self,
        role: ModelRole,
        messages: &[Value],
        tools: &[ToolDef],
        tool_choice: Option<&ToolChoice>,
        mut on_chunk: F,
    ) -> Result<()>
    where
        F: FnMut(&Value) -> Result<()>,
    {
        let plan = self.build_request_plan(role)?;

        let mut body = serde_json::json!({
            "model": plan.model.id,
            "messages": messages,
            "max_tokens": plan.model.max_tokens,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::to_value(tools)?;
        }
        if let Some(tc) = tool_choice {
            body["tool_choice"] = serde_json::to_value(tc)?;
        }
        merge_extras(&mut body, plan.extra_completion_properties.as_deref())?;

        debug!(
            messages = messages.len(),
            tools = tools.len(),
            url = %plan.url,
            model = %plan.model.id,
            "calling chat completions"
        );
        trace!(body = %body, "chat completions request body");

        let mut request = self
            .http
            .post(plan.url)
            .timeout(Duration::from_millis(plan.model.stream_idle_timeout_ms))
            .bearer_auth(&plan.bearer_token)
            .json(&body);
        for (k, v) in &plan.headers {
            request = request.header(k, v);
        }

        let response = request
            .send()
            .await
            .context("chat completion request failed")?;
        trace!(status = %response.status(), headers = ?response.headers(), "chat completions response head");

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("provider returned {status}: {text}"));
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("chat stream chunk")?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(idx) = buffer.find("\n\n") {
                let frame: String = buffer.drain(..idx + 2).collect();
                for line in frame.lines() {
                    let Some(payload) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let payload = payload.trim();
                    if payload.is_empty() || payload == "[DONE]" {
                        continue;
                    }

                    trace!(payload, "chat completions sse chunk");
                    let value: Value = match serde_json::from_str(payload) {
                        Ok(value) => value,
                        Err(error) => {
                            trace!(%error, payload, "skipping unparsable sse payload");
                            continue;
                        }
                    };

                    on_chunk(&value)?;
                }
            }
        }

        Ok(())
    }

    /// One-shot wrapper around [`stream`](Self::stream): drives the SSE
    /// stream to completion and returns the full assistant text plus any
    /// finalized tool calls. Use this when the caller doesn't need to
    /// surface mid-stream deltas (e.g. the shell classifier).
    pub async fn complete(
        &self,
        role: ModelRole,
        messages: &[Value],
        tools: &[ToolDef],
        tool_choice: Option<&ToolChoice>,
    ) -> Result<CompletionOutcome> {
        let mut text = String::new();
        let mut accumulator = ToolCallAccumulator::new();
        self.stream(role, messages, tools, tool_choice, |chunk| {
            for delta in chunk_text_deltas(chunk) {
                text.push_str(delta);
            }
            for delta in chunk_tool_call_deltas(chunk) {
                accumulator.push(delta)?;
            }
            Ok(())
        })
        .await?;
        Ok(CompletionOutcome {
            text,
            tool_calls: accumulator.finalize()?,
        })
    }

    fn build_request_plan(&self, role: ModelRole) -> Result<RequestPlan> {
        let models = self
            .models
            .get()
            .ok_or_else(|| anyhow!("models config missing"))?;
        let model: ModelConfig = match role {
            ModelRole::Chat => models.chat.clone(),
            ModelRole::ShellClassify => models
                .shell_classify
                .clone()
                .ok_or_else(|| anyhow!("models.shell_classify is not configured"))?,
        };

        let providers = self
            .providers
            .get()
            .ok_or_else(|| anyhow!("model_providers config missing"))?;
        let provider =
            case_insensitive_lookup(&providers, &model.model_provider).ok_or_else(|| {
                anyhow!(
                    "model_providers.{} is not configured (referenced by models.{}.model_provider)",
                    model.model_provider,
                    role.config_key()
                )
            })?;

        let bearer_token = resolve_bearer(&provider.auth, &self.env)?;

        let url = provider
            .base_url
            .join("chat/completions")
            .context("join base_url with chat/completions")?;

        let headers = expand_headers(&provider.http_headers, &self.env)?;

        let extras = self.extras.get_or_default();
        let extra_completion_properties = case_insensitive_lookup(&extras, &model.id)
            .and_then(|e| e.extra_completion_properties.clone());

        Ok(RequestPlan {
            url,
            bearer_token,
            headers,
            model,
            extra_completion_properties,
        })
    }
}

/// Selects which `models.<role>` entry the request targets. New roles get a
/// variant here (and a matching arm in `build_request_plan`) — providers and
/// extras lookups are shared across all roles.
#[derive(Clone, Copy, Debug)]
pub enum ModelRole {
    Chat,
    ShellClassify,
}

impl ModelRole {
    fn config_key(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::ShellClassify => "shell_classify",
        }
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

struct RequestPlan {
    url: Url,
    bearer_token: String,
    headers: Vec<(String, String)>,
    model: ModelConfig,
    extra_completion_properties: Option<String>,
}

fn case_insensitive_lookup<'a, V>(map: &'a HashMap<String, V>, key: &str) -> Option<&'a V> {
    if let Some(v) = map.get(key) {
        return Some(v);
    }
    map.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

fn resolve_bearer(auth: &AuthMethod, env: &[(OsString, OsString)]) -> Result<String> {
    match auth {
        AuthMethod::EnvKey {
            env_key,
            env_key_instructions,
        } => env
            .iter()
            .find(|(k, _)| k == env_key.as_str())
            .map(|(_, v)| v.to_string_lossy().into_owned())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| match env_key_instructions {
                Some(msg) => anyhow!("{env_key} not set in client environment — {msg}"),
                None => anyhow!("{env_key} not set in client environment"),
            }),
        AuthMethod::Token { token } => Ok(token.clone()),
        AuthMethod::File { file } => std::fs::read_to_string(file)
            .with_context(|| format!("read auth file {}", file.display()))
            .map(|s| s.trim().to_owned()),
        AuthMethod::Command { .. } => Err(anyhow!("command-backed auth is not implemented yet")),
    }
}

fn expand_headers(
    raw: &BTreeMap<String, frances_config::EnvString>,
    env: &dyn EnvLookup,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(raw.len());
    for (name, template) in raw {
        if name.eq_ignore_ascii_case("authorization") {
            warn!(
                header = %name,
                "Authorization header in http_headers is ignored — auth resolves it"
            );
            continue;
        }
        let value = template
            .expand(env)
            .with_context(|| format!("expand header {name}"))?;
        out.push((name.clone(), value));
    }
    Ok(out)
}

fn merge_extras(body: &mut Value, extras: Option<&str>) -> Result<()> {
    let Some(extras) = extras else {
        return Ok(());
    };
    let parsed: Value = serde_json::from_str(extras)
        .context("parse responses_models.<id>.extra_completion_properties as JSON")?;
    let Value::Object(extras_obj) = parsed else {
        return Err(anyhow!(
            "extra_completion_properties must be a JSON object, got {}",
            type_name_of(&parsed)
        ));
    };
    let Value::Object(body_obj) = body else {
        unreachable!("body is constructed as a JSON object above");
    };
    for (k, v) in extras_obj {
        body_obj.insert(k, v);
    }
    Ok(())
}

fn type_name_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
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
                        {"index": 0, "id": "a", "function": {"name": "read_file"}},
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
                name: "read_file",
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
        assert_eq!(calls[0].name, "read_file");
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

    #[test]
    fn merge_extras_overrides_existing_keys() {
        let mut body = json!({
            "model": "qwen",
            "max_tokens": 1000,
        });
        merge_extras(
            &mut body,
            Some(r#"{"max_tokens": 2000, "provider": {"order": ["parasail"]}}"#),
        )
        .unwrap();
        assert_eq!(body["max_tokens"], 2000);
        assert_eq!(body["provider"]["order"][0], "parasail");
        assert_eq!(body["model"], "qwen");
    }

    #[test]
    fn merge_extras_rejects_non_object() {
        let mut body = json!({});
        let err = merge_extras(&mut body, Some(r#"["nope"]"#)).unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"));
    }

    #[test]
    fn merge_extras_none_is_noop() {
        let mut body = json!({"a": 1});
        merge_extras(&mut body, None).unwrap();
        assert_eq!(body, json!({"a": 1}));
    }
}
