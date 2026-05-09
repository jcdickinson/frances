use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use frances_config::EnvLookup;
use futures_util::StreamExt;
use serde_json::Value;
use thiserror::Error as ThisError;
use tracing::{debug, trace};
use url::Url;

use crate::config::{AuthMethod, ModelConfig, ProviderConfig, ResponsesModelExtras};
use crate::provider::{
    self, CompletionOutcome, ErasedError, ProviderRequest, StreamEvent, ToolCall, Usage,
};

/// OpenAI-style chat-completions provider.
pub struct Provider {
    provider_config: ProviderConfig,
    extras: ResponsesModelExtras,
    http: reqwest::Client,
}

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("build reqwest client: {0}")]
    BuildClient(#[source] reqwest::Error),
    #[error("invalid base_url: {0}")]
    JoinBaseUrl(#[source] url::ParseError),
    #[error("env var '{0}' not set in client environment")]
    MissingEnvVar(String),
    #[error("env var '{var}' not set in client environment — {hint}")]
    MissingEnvVarHinted { var: String, hint: String },
    #[error("read auth file {path}: {source}")]
    ReadAuthFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("command-backed auth is not implemented yet")]
    AuthCommandUnimplemented,
    #[error("expand header {name}: {source}")]
    ExpandHeader {
        name: String,
        #[source]
        source: frances_config::EnvStringExpandError,
    },
    #[error("serialize tool definitions: {0}")]
    SerializeTools(#[source] serde_json::Error),
    #[error("serialize tool_choice: {0}")]
    SerializeToolChoice(#[source] serde_json::Error),
    #[error("parse extra_completion_properties as JSON: {0}")]
    ParseExtras(#[source] serde_json::Error),
    #[error("extra_completion_properties must be a JSON object, got {0}")]
    ExtrasNotObject(&'static str),
    #[error("HTTP request failed: {0}")]
    Http(#[source] reqwest::Error),
    #[error("provider returned {status}: {body}")]
    BadStatus {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("read stream chunk: {0}")]
    StreamChunk(#[source] reqwest::Error),
    #[error("on_event callback aborted: {0}")]
    OnEvent(ErasedError),
    #[error("tool call accumulator: {0}")]
    Accumulator(#[from] ToolCallError),
}

impl From<ErasedError> for Error {
    fn from(e: ErasedError) -> Self {
        Self::OnEvent(e)
    }
}

#[derive(Debug, ThisError)]
pub enum ToolCallError {
    #[error("tool call at index {0} already started")]
    AlreadyStarted(u32),
    #[error("argument fragment for unstarted tool call at index {0}")]
    AppendBeforeStart(u32),
    #[error("parse arguments for tool call {id} ({name}): {source}")]
    ParseArguments {
        id: String,
        name: String,
        #[source]
        source: serde_json::Error,
    },
}

#[async_trait]
impl provider::Provider for Provider {
    type Extras = ResponsesModelExtras;
    type BuildError = Error;
    type Error = Error;

    fn new(
        provider_config: ProviderConfig,
        extras: ResponsesModelExtras,
    ) -> std::result::Result<Arc<Self>, Error> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(Error::BuildClient)?;
        Ok(Arc::new(Self {
            provider_config,
            extras,
            http,
        }))
    }

    async fn stream(
        &self,
        req: ProviderRequest<'_>,
        on_event: &mut (dyn FnMut(StreamEvent) -> std::result::Result<(), Error> + Send),
    ) -> std::result::Result<CompletionOutcome, Error> {
        let _ = req.session_id; // OpenAI auto-caches; we don't need to thread the id today.
        let plan = self.build_request_plan(req.model, req.env)?;

        let mut body = serde_json::json!({
            "model": plan.model.id,
            "messages": req.messages,
            "max_tokens": plan.model.max_tokens,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !req.tools.is_empty() {
            body["tools"] = serde_json::to_value(req.tools).map_err(Error::SerializeTools)?;
        }
        if let Some(tc) = req.tool_choice {
            body["tool_choice"] = serde_json::to_value(tc).map_err(Error::SerializeToolChoice)?;
        }
        merge_extras(&mut body, plan.extra_completion_properties.as_deref())?;

        debug!(
            messages = req.messages.len(),
            tools = req.tools.len(),
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

        let response = request.send().await.map_err(Error::Http)?;
        trace!(status = %response.status(), headers = ?response.headers(), "chat completions response head");

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::BadStatus { status, body });
        }

        let mut bytes = response.bytes_stream();
        let mut buffer = String::new();

        let mut text = String::new();
        let mut accumulator = ToolCallAccumulator::new();

        while let Some(chunk) = bytes.next().await {
            let bytes = chunk.map_err(Error::StreamChunk)?;
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

                    for delta in chunk_text_deltas(&value) {
                        text.push_str(delta);
                        on_event(StreamEvent::TextDelta(delta.to_owned()))?;
                    }
                    for tcd in chunk_tool_call_deltas(&value) {
                        accumulator.push(tcd)?;
                    }
                    if let Some(usage) = chunk_usage(&value) {
                        on_event(StreamEvent::Usage(usage))?;
                    }
                }
            }
        }

        let tool_calls = accumulator.finalize()?;
        for call in &tool_calls {
            on_event(StreamEvent::ToolCall(call.clone()))?;
        }
        Ok(CompletionOutcome { text, tool_calls })
    }
}

impl Provider {
    fn build_request_plan(
        &self,
        model: &ModelConfig,
        env: &HashMap<OsString, OsString>,
    ) -> std::result::Result<RequestPlan, Error> {
        let bearer_token = resolve_bearer(&self.provider_config.auth, env)?;
        let url = self
            .provider_config
            .base_url
            .join("chat/completions")
            .map_err(Error::JoinBaseUrl)?;
        let headers = expand_headers(&self.provider_config.http_headers, env)?;
        let extra_completion_properties = self.extras.extra_completion_properties.clone();
        Ok(RequestPlan {
            url,
            bearer_token,
            headers,
            extra_completion_properties,
            model: model.clone(),
        })
    }
}

struct RequestPlan {
    url: Url,
    bearer_token: String,
    headers: Vec<(String, String)>,
    model: ModelConfig,
    extra_completion_properties: Option<String>,
}

fn resolve_bearer(
    auth: &AuthMethod,
    env: &HashMap<OsString, OsString>,
) -> std::result::Result<String, Error> {
    match auth {
        AuthMethod::EnvKey {
            env_key,
            env_key_instructions,
        } => env
            .get(std::ffi::OsStr::new(env_key))
            .map(|v| v.to_string_lossy().into_owned())
            .ok_or_else(|| match env_key_instructions {
                Some(hint) => Error::MissingEnvVarHinted {
                    var: env_key.clone(),
                    hint: hint.clone(),
                },
                None => Error::MissingEnvVar(env_key.clone()),
            }),
        AuthMethod::Token { token } => Ok(token.clone()),
        AuthMethod::File { file } => std::fs::read_to_string(file)
            .map(|s| s.trim().to_owned())
            .map_err(|source| Error::ReadAuthFile {
                path: file.clone(),
                source,
            }),
        AuthMethod::Command { .. } => Err(Error::AuthCommandUnimplemented),
    }
}

fn expand_headers(
    raw: &BTreeMap<String, frances_config::EnvString>,
    env: &dyn EnvLookup,
) -> std::result::Result<Vec<(String, String)>, Error> {
    let mut out = Vec::with_capacity(raw.len());
    for (name, template) in raw {
        if name.eq_ignore_ascii_case("authorization") {
            tracing::warn!(
                header = %name,
                "Authorization header in http_headers is ignored — auth resolves it"
            );
            continue;
        }
        let value = template.expand(env).map_err(|source| Error::ExpandHeader {
            name: name.clone(),
            source,
        })?;
        out.push((name.clone(), value));
    }
    Ok(out)
}

fn merge_extras(body: &mut Value, extras: Option<&str>) -> std::result::Result<(), Error> {
    let Some(extras) = extras else {
        return Ok(());
    };
    let parsed: Value = serde_json::from_str(extras).map_err(Error::ParseExtras)?;
    let Value::Object(extras_obj) = parsed else {
        return Err(Error::ExtrasNotObject(type_name_of(&parsed)));
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

// ---------------------------------------------------------------------------
// Internal SSE chunk parsers + tool-call accumulator (OpenAI wire shape)
// ---------------------------------------------------------------------------

fn chunk_text_deltas(chunk: &Value) -> impl Iterator<Item = &str> {
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

#[derive(Debug, Clone, Copy)]
struct ToolCallDelta<'a> {
    index: u32,
    event: ToolCallEvent<'a>,
}

#[derive(Debug, Clone, Copy)]
enum ToolCallEvent<'a> {
    Start { id: &'a str, name: &'a str },
    Append(&'a str),
}

fn chunk_tool_call_deltas(chunk: &Value) -> Vec<ToolCallDelta<'_>> {
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

fn chunk_usage(chunk: &Value) -> Option<Usage> {
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
        cached_input_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    })
}

struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct ToolCallAccumulator {
    in_progress: std::collections::BTreeMap<u32, ToolCallBuilder>,
}

impl ToolCallAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, delta: ToolCallDelta<'_>) -> Result<(), ToolCallError> {
        match delta.event {
            ToolCallEvent::Start { id, name } => {
                if self.in_progress.contains_key(&delta.index) {
                    return Err(ToolCallError::AlreadyStarted(delta.index));
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
                let builder = self
                    .in_progress
                    .get_mut(&delta.index)
                    .ok_or(ToolCallError::AppendBeforeStart(delta.index))?;
                builder.arguments.push_str(fragment);
            }
        }
        Ok(())
    }

    fn finalize(self) -> Result<Vec<ToolCall>, ToolCallError> {
        self.in_progress
            .into_values()
            .map(|b| {
                let arguments: Value =
                    serde_json::from_str(&b.arguments).map_err(|source| {
                        ToolCallError::ParseArguments {
                            id: b.id.clone(),
                            name: b.name.clone(),
                            source,
                        }
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
        assert!(matches!(err, ToolCallError::AppendBeforeStart(0)));
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
        assert!(matches!(err, ToolCallError::AlreadyStarted(0)));
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
        assert!(matches!(err, ToolCallError::ParseArguments { .. }));
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
        assert!(matches!(err, Error::ExtrasNotObject(_)));
    }

    #[test]
    fn merge_extras_none_is_noop() {
        let mut body = json!({"a": 1});
        merge_extras(&mut body, None).unwrap();
        assert_eq!(body, json!({"a": 1}));
    }
}
