use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;
use thiserror::Error as ThisError;
use tracing::{debug, trace};

use crate::config::{ProviderConfig, ResponsesModelExtras};
use crate::provider::{
    self, CompletionOutcome, ErasedError, HistoryInput, ProviderRequest, StreamEvent, ToolCall,
};

mod request_plan;
mod sse;
mod tool_calls;

use request_plan::RequestPlan;
use tool_calls::ToolCallAccumulator;

pub use tool_calls::ToolCallError;

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
    #[error(transparent)]
    RequestPlan(#[from] request_plan::Error),
    #[error("serialize tool definitions: {0}")]
    SerializeTools(#[source] serde_json::Error),
    #[error("serialize tool_choice: {0}")]
    SerializeToolChoice(#[source] serde_json::Error),
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

#[async_trait]
impl provider::Provider for Provider {
    type Extras = ResponsesModelExtras;
    type BuildError = Error;
    type Error = Error;

    fn kind(&self) -> &'static str {
        "openai-chat-completions"
    }

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

    fn forge_history(&self, inputs: &[HistoryInput<'_>]) -> Vec<Value> {
        inputs
            .iter()
            .map(|input| match input {
                HistoryInput::User { text } => {
                    serde_json::json!({ "role": "user", "content": text })
                }
                HistoryInput::Assistant { text } => {
                    serde_json::json!({ "role": "assistant", "content": text })
                }
                HistoryInput::ToolCall {
                    id,
                    name,
                    arguments,
                } => serde_json::json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(arguments).unwrap_or_default(),
                        }
                    }]
                }),
                HistoryInput::ToolResult {
                    call_id,
                    content,
                    is_error: _,
                } => serde_json::json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content,
                }),
            })
            .collect()
    }

    async fn stream(
        &self,
        req: ProviderRequest<'_>,
        on_event: &mut (dyn FnMut(StreamEvent) -> std::result::Result<(), Error> + Send),
    ) -> std::result::Result<CompletionOutcome, Error> {
        let _ = req.session_id; // OpenAI auto-caches; we don't need to thread the id today.
        let plan = RequestPlan::build(&self.provider_config, &self.extras, req.model, req.env)?;

        // Forge new_inputs upfront, emit one History event per output, then
        // assemble the request body's messages array as `req.history` ++
        // forged_new_inputs.
        let forged_new = self.forge_history(req.new_inputs);
        for payload in &forged_new {
            on_event(StreamEvent::History(payload.clone()))?;
        }
        let mut messages: Vec<Value> = Vec::with_capacity(req.history.len() + forged_new.len());
        messages.extend(req.history.iter().cloned());
        messages.extend(forged_new);

        let mut body = serde_json::json!({
            "model": plan.model.id,
            "messages": messages,
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
        request_plan::merge_extras(&mut body, plan.extra_completion_properties.as_deref())?;

        debug!(
            messages = messages.len(),
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

                    for delta in sse::chunk_text_deltas(&value) {
                        text.push_str(delta);
                        on_event(StreamEvent::TextDelta(delta.to_owned()))?;
                    }
                    for tcd in sse::chunk_tool_call_deltas(&value) {
                        accumulator.push(tcd)?;
                    }
                    if let Some(usage) = sse::chunk_usage(&value) {
                        on_event(StreamEvent::Usage(usage))?;
                    }
                }
            }
        }

        let tool_calls = accumulator.finalize()?;
        // Emit one consolidated assistant History event covering the text
        // and any tool_calls. (For OpenAI's wire, all of these belong to
        // a single assistant message.)
        let assistant_payload = build_assistant_payload(&text, &tool_calls);
        on_event(StreamEvent::History(assistant_payload))?;
        for call in &tool_calls {
            on_event(StreamEvent::ToolCall(call.clone()))?;
        }
        Ok(CompletionOutcome { text, tool_calls })
    }
}

fn build_assistant_payload(text: &str, tool_calls: &[ToolCall]) -> Value {
    let content = if text.is_empty() {
        Value::Null
    } else {
        Value::String(text.to_string())
    };
    let mut payload = serde_json::json!({
        "role": "assistant",
        "content": content,
    });
    if !tool_calls.is_empty() {
        let calls: Vec<Value> = tool_calls
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": serde_json::to_string(&c.arguments).unwrap_or_default(),
                    }
                })
            })
            .collect();
        payload
            .as_object_mut()
            .expect("payload is object")
            .insert("tool_calls".to_string(), Value::Array(calls));
    }
    payload
}
