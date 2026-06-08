//! Multi-adapter LLM provider via the `genai` crate.
//!
//! One `Provider` impl, parameterised on a `genai::adapter::AdapterKind`
//! resolved from `ProviderConfig.kind`. The cache builds one Provider
//! per `model_providers.<id>` config entry; each instance reports its
//! wire name (e.g. `"openai-chat"`, `"anthropic"`, `"zai"`) via
//! `Provider::kind()` for persistence tagging.
//!
//! `ProviderConfig.base_url` is honoured by injecting a
//! `ServiceTargetResolver` into the genai `Client` that overrides the
//! adapter's default endpoint. The bearer resolves per call against the
//! latest invocation's env via an `AuthResolver`.
//!
//! Reasoning round-trips natively: genai's `ContentPart::ReasoningContent`
//! is hoisted back into each adapter's wire format on subsequent
//! requests, so the next-turn prefix matches the prompt cache.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use genai::Client;
use genai::ModelIden;
use genai::ServiceTarget;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatRole, ChatStreamEvent, ContentPart, MessageContent,
    Tool as GenaiTool, ToolCall as GenaiToolCall, ToolName, ToolResponse,
};
use genai::resolver::{AuthData, AuthResolver, Endpoint, ServiceTargetResolver};
use serde_json::Value;
use thiserror::Error as ThisError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};
use uuid::Uuid;

use frances_config::{ConfigBinding, ConfigHandle};
use frances_core::Truncated;
use frances_models_llm::config::{OpenRouterConfig, ProviderConfig};
use frances_models_llm::{
    CompletionOutcome, ErasedError, HistoryInput, StreamEvent, ToolCall, ToolChoice, ToolDef, Usage,
};

use crate::provider::{self, ProviderRequest};

mod kinds;
mod request_plan;

use kinds::parse_kind;
use request_plan::RequestPlan;

pub struct Provider {
    provider_config: ProviderConfig,
    /// The canonical wire-name returned by `Provider::kind()`.
    /// Statically-allocated for lifetime reasons (the trait method
    /// signature requires `&'static str`).
    kind: &'static str,
    /// genai adapter the resolved kind maps to. Bound on every `Client`
    /// we build via `ClientBuilder::with_adapter_kind`.
    adapter: AdapterKind,
    /// Live binding to the top-level `[openrouter]` config block — only
    /// populated when this provider's `kind` is `openrouter`.
    openrouter: Option<ConfigBinding<OpenRouterConfig>>,
    /// One shared `reqwest::Client` (internally `Arc`'d) reused across every
    /// stream this provider runs, so sequential completions to the same
    /// endpoint reuse the connection pool + warm keep-alive instead of paying
    /// a fresh TCP+TLS handshake per request. The per-plan auth/target
    /// resolvers still rebuild each call — only the pool survives.
    http: reqwest::Client,
}

#[derive(Debug, ThisError)]
pub enum Error {
    #[error(transparent)]
    Kinds(#[from] kinds::Error),
    #[error(transparent)]
    RequestPlan(#[from] request_plan::Error),
    #[error("genai: {0}")]
    GenAI(#[source] genai::Error),
    #[error("bind {path}: {source}")]
    Bind {
        path: &'static str,
        #[source]
        source: frances_config::ConfigBindError,
    },
    #[error("history row {index} is not a chat message: {source}")]
    DecodeHistory {
        index: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to encode chat message for the request: {0}")]
    EncodeHistory(#[source] serde_json::Error),
    #[error("on_event callback aborted: {0}")]
    OnEvent(ErasedError),
    #[error("cancelled")]
    Cancelled,
}

impl From<ErasedError> for Error {
    fn from(e: ErasedError) -> Self {
        Self::OnEvent(e)
    }
}

/// Total attempts for one chat call (1 initial + retries) when the
/// failure is transient and lands before any model output.
const MAX_STREAM_ATTEMPTS: u32 = 4;

/// Backoff before the next attempt, given the attempt number that just
/// failed (1-based): 250ms, 500ms, 1s.
fn retry_backoff(failed_attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(250 * (1u64 << (failed_attempt - 1)))
}

/// Whether an error is a transient network/server fault worth retrying.
/// Conservative: 5xx, 429, and dropped connections/streams only — never
/// 4xx (bad request / auth) or our own encode/decode/cancel errors,
/// which would fail identically on retry.
fn is_transient(err: &Error) -> bool {
    match err {
        Error::GenAI(e) => genai_transient(e),
        _ => false,
    }
}

fn genai_transient(e: &genai::Error) -> bool {
    use genai::Error as G;
    match e {
        G::HttpError { status, .. } => transient_status(*status),
        // A broken stream is safe to retry *before any output* (the only
        // place `is_transient` is consulted). genai collapses the cause to
        // a string here, so we can't see the status — but a stream that
        // died before emitting anything is exactly what we want to retry.
        G::WebStream { .. } => true,
        G::WebModelCall { webc_error, .. } | G::WebAdapterCall { webc_error, .. } => {
            webc_transient(webc_error)
        }
        _ => false,
    }
}

fn webc_transient(e: &genai::webc::Error) -> bool {
    use genai::webc::Error as W;
    match e {
        W::Reqwest(_) => true,
        W::ResponseFailedStatus { status, .. } => transient_status(*status),
        _ => false,
    }
}

fn transient_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

#[async_trait]
impl provider::Provider for Provider {
    type BuildError = Error;
    type Error = Error;

    fn kind(&self) -> &'static str {
        self.kind
    }

    fn new(
        provider_config: ProviderConfig,
        handle: ConfigHandle,
    ) -> std::result::Result<Arc<Self>, Error> {
        let (kind, adapter) = parse_kind(&provider_config.kind)?;
        let openrouter = if matches!(adapter, AdapterKind::OpenRouter) {
            Some(
                handle
                    .bind::<OpenRouterConfig>("openrouter")
                    .map_err(|source| Error::Bind {
                        path: "openrouter",
                        source,
                    })?,
            )
        } else {
            None
        };
        Ok(Arc::new(Self {
            provider_config,
            kind,
            adapter,
            openrouter,
            http: reqwest::Client::new(),
        }))
    }

    fn forge_history(
        &self,
        inputs: &[HistoryInput<'_>],
    ) -> std::result::Result<Vec<Value>, serde_json::Error> {
        inputs
            .iter()
            .map(|input| serde_json::to_value(forge_one(input)))
            .collect()
    }

    async fn stream(
        &self,
        req: ProviderRequest<'_>,
        cancel: CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) -> std::result::Result<(), Error> + Send),
    ) -> std::result::Result<CompletionOutcome, Error> {
        let _ = req.session_id; // future: thread through prompt_cache_key on ChatOptions.
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let max_tool_calls = req.max_tool_calls;
        let qwen_quirks = self.qwen_quirks_for(req.model_name);
        let plan = RequestPlan::build(&self.provider_config, req.model, req.env)?;

        // Trace incoming tool-related inputs before forge so the round-
        // trip with the model is visible in the session runtime trace stream.
        for input in req.new_inputs {
            match input {
                HistoryInput::ToolCall {
                    id,
                    name,
                    arguments,
                } => trace!(
                    call_id = %id,
                    name = %name,
                    arguments = %arguments,
                    "tool call to model (forged from primitive)",
                ),
                HistoryInput::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => trace!(
                    call_id = %call_id,
                    is_error = %is_error,
                    content = %content,
                    "tool result to model",
                ),
                _ => {}
            }
        }

        let forged_new = self
            .forge_history(req.new_inputs)
            .map_err(Error::EncodeHistory)?;
        for payload in &forged_new {
            on_event(StreamEvent::History(payload.clone()))?;
        }

        let mut messages: Vec<ChatMessage> =
            Vec::with_capacity(req.history.len() + forged_new.len());
        for (i, v) in req.history.iter().chain(forged_new.iter()).enumerate() {
            let msg: ChatMessage = serde_json::from_value(v.clone())
                .map_err(|source| Error::DecodeHistory { index: i, source })?;
            messages.push(msg);
        }
        remap_tool_call_ids(&mut messages);

        let chat_req = build_chat_request(messages, req.tools, req.tool_choice);
        let chat_options = build_chat_options(&plan, req.tool_choice);
        let model_id = plan.model.id.clone();

        let client = build_client(self.adapter, &plan, &self.http)?;

        // Serialising the whole request (full history) is not free, so only
        // pay for it when TRACE is actually live. Cap is large enough that a
        // typical multi-turn body fits even with several tool messages. The
        // body is identical across retries, so trace it once.
        if tracing::enabled!(tracing::Level::TRACE)
            && let Ok(s) = serde_json::to_string(&chat_req)
        {
            trace!(body = %Truncated::<20000>::new(s), "chat request body");
        }

        // Transparent transient-failure retry. Retrying is only safe while NO
        // model output has reached `on_event` yet: once a TextDelta /
        // ReasoningDelta / ToolCall / History has been emitted, a retry would
        // re-stream it (duplicate text on screen, duplicate tool calls in
        // history). `emitted` gates that — it only flips false→true, so any
        // error after the first output is terminal. History for `new_inputs`
        // was already emitted above, once, before this loop.
        let mut emitted = false;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            debug!(
                adapter = ?self.adapter,
                messages = chat_req.messages.len(),
                tools = req.tools.len(),
                base_url = %plan.base_url,
                model = %plan.model.id,
                attempt,
                "calling genai chat stream"
            );

            let result: std::result::Result<CompletionOutcome, Error> = async {
                let response = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(Error::Cancelled),
                    res = client.exec_chat_stream(model_id.as_str(), chat_req.clone(), Some(&chat_options))
                        => res.map_err(Error::GenAI)?,
                };
                let mut stream = response.stream;

                let mut text = String::new();
                let mut reasoning_text = String::new();
                let mut thought_signatures: Vec<String> = Vec::new();
                let mut tool_calls: Vec<ToolCall> = Vec::new();
                let mut final_usage: Option<Usage> = None;

                loop {
                    let event = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(Error::Cancelled),
                        next = stream.next() => match next {
                            Some(Ok(ev)) => ev,
                            Some(Err(e)) => return Err(Error::GenAI(e)),
                            None => break,
                        },
                    };

                    match event {
                        ChatStreamEvent::Start => {}
                        ChatStreamEvent::Chunk(chunk) if !chunk.content.is_empty() => {
                            text.push_str(&chunk.content);
                            emitted = true;
                            on_event(StreamEvent::TextDelta(chunk.content))?;
                        }
                        ChatStreamEvent::Chunk(_) => {}
                        ChatStreamEvent::ReasoningChunk(chunk) if !chunk.content.is_empty() => {
                            // Reasoning rides its own channel — consumers (TUI,
                            // step-transcript summariser) treat it differently
                            // from response text. It's also retained verbatim for
                            // the assistant `reasoning_content` round-trip.
                            reasoning_text.push_str(&chunk.content);
                            emitted = true;
                            on_event(StreamEvent::ReasoningDelta(chunk.content))?;
                        }
                        ChatStreamEvent::ReasoningChunk(_) => {}
                        ChatStreamEvent::ThoughtSignatureChunk(chunk) if !chunk.content.is_empty() => {
                            thought_signatures.push(chunk.content);
                        }
                        ChatStreamEvent::ThoughtSignatureChunk(_) => {}
                        // genai accumulates per-chunk argument deltas
                        // internally (capture_tool_calls); the complete calls
                        // are taken from `End` below. Emitting per chunk would
                        // surface fragments to the UI and pin `emitted`,
                        // blocking otherwise-safe retries.
                        ChatStreamEvent::ToolCallChunk(_) => {}
                        ChatStreamEvent::End(end) => {
                            if let Some(u) = end.captured_usage.as_ref() {
                                final_usage = Some(map_usage(u));
                            }
                            if let Some(rc) = &end.captured_reasoning_content
                                && reasoning_text.is_empty()
                            {
                                // Adapter captured a normalised reasoning string
                                // that we missed via deltas (e.g. </think>-style
                                // post-hoc extraction). Honour it.
                                reasoning_text.push_str(rc);
                            }
                            // Take the full tool calls genai assembled across
                            // all chunks. `captured_into_tool_calls` consumes
                            // `end`, so it must come after the borrows above.
                            if let Some(captured) = end.captured_into_tool_calls() {
                                for genai_call in captured {
                                    let mut call = map_tool_call(genai_call);
                                    if qwen_quirks {
                                        frances_models_llm::tool_args::repair_qwen_quirks(
                                            &mut call, req.tools,
                                        );
                                    }
                                    tool_calls.push(call);
                                }
                                if let Some(cap) = max_tool_calls {
                                    tool_calls.truncate(cap);
                                }
                            }
                        }
                    }
                }

                // Stream completed cleanly; any failure past this point is in
                // our own post-processing, not a retryable network fault.
                emitted = true;
                for call in &tool_calls {
                    trace!(
                        call_id = %call.id,
                        name = %call.name,
                        arguments = %call.arguments,
                        "tool call from model",
                    );
                    on_event(StreamEvent::ToolCall(call.clone()))?;
                }
                if let Some(u) = final_usage {
                    on_event(StreamEvent::Usage(u))?;
                }
                let assistant = build_assistant_payload(
                    &text,
                    &tool_calls,
                    &reasoning_text,
                    &thought_signatures,
                )
                .map_err(Error::EncodeHistory)?;
                on_event(StreamEvent::History(assistant))?;

                Ok(CompletionOutcome { text, tool_calls })
            }
            .await;

            match result {
                Ok(outcome) => return Ok(outcome),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    if !emitted && attempt < MAX_STREAM_ATTEMPTS && is_transient(&e) {
                        let delay = retry_backoff(attempt);
                        warn!(
                            attempt,
                            max_attempts = MAX_STREAM_ATTEMPTS,
                            delay_ms = delay.as_millis() as u64,
                            error = %e,
                            "transient provider failure before any output; retrying"
                        );
                        tokio::select! {
                            biased;
                            () = cancel.cancelled() => return Err(Error::Cancelled),
                            () = tokio::time::sleep(delay) => {}
                        }
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
}

impl Provider {
    /// Read the per-model `qwen_quirks` flag from the live `[openrouter]`
    /// binding. Returns `false` when the provider isn't openrouter, when
    /// the config block is absent, or when the model has no entry.
    fn qwen_quirks_for(&self, model_name: &str) -> bool {
        let Some(binding) = &self.openrouter else {
            return false;
        };
        let Some(guard) = binding.get() else {
            return false;
        };
        guard
            .models
            .get(model_name)
            .map(|m| m.qwen_quirks)
            .unwrap_or(false)
    }
}

fn build_client(
    adapter: AdapterKind,
    plan: &RequestPlan,
    http: &reqwest::Client,
) -> std::result::Result<Client, Error> {
    let base_url: Arc<str> = Arc::from(plan.base_url.as_str());
    let api_key = plan.api_key.clone();
    let target_resolver = ServiceTargetResolver::from_resolver_fn(
        move |mut target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            target.endpoint = Endpoint::from_owned(base_url.clone());
            Ok(target)
        },
    );
    let auth_resolver = AuthResolver::from_resolver_fn(
        move |_model_iden: ModelIden| -> Result<Option<AuthData>, genai::resolver::Error> {
            Ok(Some(AuthData::Key(api_key.clone())))
        },
    );
    let client = Client::builder()
        .with_reqwest(http.clone())
        .with_adapter_kind(adapter)
        .with_auth_resolver(auth_resolver)
        .with_service_target_resolver(target_resolver)
        .build();
    Ok(client)
}

fn build_chat_request(
    messages: Vec<ChatMessage>,
    tools: &[ToolDef],
    _tool_choice: Option<&ToolChoice>,
) -> ChatRequest {
    let mut req = ChatRequest::new(messages);
    if !tools.is_empty() {
        req.tools = Some(tools.iter().map(tool_def_to_genai).collect());
    }
    req
}

fn build_chat_options(plan: &RequestPlan, tool_choice: Option<&ToolChoice>) -> ChatOptions {
    let mut opts = ChatOptions::default();
    if let Some(m) = plan.model.max_tokens {
        opts.max_tokens = Some(m);
    }
    opts.capture_usage = Some(true);
    opts.capture_reasoning_content = Some(true);
    opts.capture_tool_calls = Some(true);
    if !plan.extra_headers.is_empty() {
        opts.extra_headers = Some(genai::Headers::from(plan.extra_headers.clone()));
    }
    if let Some(tc) = tool_choice {
        opts.tool_choice = Some(map_tool_choice(tc));
    }
    opts
}

fn map_tool_choice(tc: &ToolChoice) -> genai::chat::ToolChoice {
    use genai::chat::ToolChoice as G;
    match tc {
        ToolChoice::Auto => G::Auto,
        ToolChoice::None => G::None,
        ToolChoice::Required => G::Required,
        ToolChoice::Function(name) => G::Tool { name: name.clone() },
    }
}

fn tool_def_to_genai(td: &ToolDef) -> GenaiTool {
    let ToolDef::Function(f) = td;
    GenaiTool {
        name: ToolName::Custom(f.name.clone()),
        description: Some(f.description.clone()),
        schema: Some(f.parameters.clone()),
        // Strict "when possible": OpenAI strict mode rejects extensible
        // schemas, so only enable it for schemas that satisfy the subset.
        strict: Some(frances_models_llm::tool_args::is_strict_compatible(
            &f.parameters,
        )),
        config: None,
    }
}

fn map_tool_call(call: GenaiToolCall) -> ToolCall {
    // genai's OpenAI-shaped streamer (adapter_shared `capture_tool_call`)
    // accumulates `fn_arguments` as `Value::String(raw_json)` during the
    // stream — only the post-`[DONE]` `captured_data.tool_calls` path
    // parses the string into a proper `Value::Object`. Per-event
    // `ToolCallChunk`s reach us with the unparsed string, so we parse
    // here. Fall back to the original Value on parse failure (matches
    // genai's own resilience pattern).
    let arguments = match call.fn_arguments {
        Value::String(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)),
        other => other,
    };
    ToolCall {
        error: None,
        id: call.call_id,
        name: call.fn_name,
        arguments,
    }
}

fn map_usage(u: &genai::chat::Usage) -> Usage {
    Usage {
        prompt_tokens: u.prompt_tokens.unwrap_or(0).max(0) as u32,
        completion_tokens: u.completion_tokens.unwrap_or(0).max(0) as u32,
        total_tokens: u.total_tokens.unwrap_or(0).max(0) as u32,
        cached_input_tokens: u
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| serde_json::to_value(d).ok())
            .and_then(|v| {
                v.get("cached_tokens")
                    .and_then(serde_json::Value::as_i64)
                    .map(|n| n.max(0) as u32)
            })
            .unwrap_or(0),
    }
}

/// Rewrite every tool-call id in the assembled request to a request-unique
/// GUID, propagating each rewrite to the matching tool result.
///
/// genai synthesises ids from a per-response index (`call_0`, `call_1`) for
/// providers that omit them (e.g. DeepSeek), so the same id recurs every
/// turn; replaying the full history then sends duplicate `tool_call_id`s in
/// one request and the provider rejects it. The GUID is derived from the
/// call's ordinal position, which is stable across turns (history is
/// append-only), so the rewritten ids are identical request-to-request and
/// the prompt-cache prefix doesn't shift.
fn remap_tool_call_ids(messages: &mut [ChatMessage]) {
    // Fixed namespace for deterministic v5 derivation.
    const TOOL_CALL_NS: Uuid = Uuid::from_u128(0x6f3c_2b1a_8d4e_4f7a_9b2c_1e5d_7a9f_0c3b);
    let mut ordinal: u32 = 0;
    let mut rename: HashMap<String, String> = HashMap::new();
    for message in messages.iter_mut() {
        for part in message.content.iter_mut() {
            match part {
                ContentPart::ToolCall(call) => {
                    let guid = Uuid::new_v5(&TOOL_CALL_NS, ordinal.to_string().as_bytes());
                    ordinal += 1;
                    let guid = guid.to_string();
                    // A result always follows its call, and a given old id is
                    // reused only in a later turn — so overwriting an earlier
                    // mapping here is safe: that turn's result was already
                    // rewritten before we reach this point.
                    rename.insert(call.call_id.clone(), guid.clone());
                    call.call_id = guid;
                }
                ContentPart::ToolResponse(response) => {
                    if let Some(guid) = rename.get(&response.call_id) {
                        response.call_id = guid.clone();
                    }
                }
                _ => {}
            }
        }
    }
}

/// Map one `HistoryInput` primitive to a `ChatMessage`.
fn forge_one(input: &HistoryInput<'_>) -> ChatMessage {
    match input {
        HistoryInput::System { text } => ChatMessage::system(text.to_string()),
        HistoryInput::User { text } => ChatMessage::user(text.to_string()),
        HistoryInput::Assistant { text } => ChatMessage::assistant(text.to_string()),
        HistoryInput::ToolCall {
            id,
            name,
            arguments,
        } => {
            let call = GenaiToolCall {
                call_id: id.to_string(),
                fn_name: name.to_string(),
                fn_arguments: (*arguments).clone(),
                thought_signatures: None,
            };
            ChatMessage::assistant_tool_calls_with_thoughts(vec![call], Vec::new())
        }
        HistoryInput::ToolResult {
            call_id,
            content,
            is_error: _,
        } => {
            let response = ToolResponse::new(call_id.to_string(), content.to_string());
            ChatMessage::tool(MessageContent::from_parts(vec![ContentPart::ToolResponse(
                response,
            )]))
        }
    }
}

/// Build the assistant turn's History payload as a serialised genai
/// `ChatMessage`. Includes reasoning_content and thought signatures so
/// the next request prefix matches what the model emitted.
fn build_assistant_payload(
    text: &str,
    tool_calls: &[ToolCall],
    reasoning_text: &str,
    thought_signatures: &[String],
) -> std::result::Result<Value, serde_json::Error> {
    let mut parts: Vec<ContentPart> = Vec::new();
    for sig in thought_signatures {
        parts.push(ContentPart::ThoughtSignature(sig.clone()));
    }
    if !text.is_empty() {
        parts.push(ContentPart::Text(text.to_owned()));
    }
    if !reasoning_text.is_empty() {
        parts.push(ContentPart::ReasoningContent(reasoning_text.to_owned()));
    }
    for call in tool_calls {
        parts.push(ContentPart::ToolCall(GenaiToolCall {
            call_id: call.id.clone(),
            fn_name: call.name.clone(),
            fn_arguments: call.arguments.clone(),
            thought_signatures: None,
        }));
    }
    let content = if parts.is_empty() {
        MessageContent::from(String::new())
    } else {
        MessageContent::from_parts(parts)
    };
    let msg = ChatMessage {
        role: ChatRole::Assistant,
        content,
        options: None,
    };
    serde_json::to_value(&msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn forge_user_round_trips_through_chat_message() {
        let v = serde_json::to_value(forge_one(&HistoryInput::User { text: "hi" })).unwrap();
        let back: ChatMessage = serde_json::from_value(v).unwrap();
        assert!(matches!(back.role, ChatRole::User));
    }

    #[test]
    fn forge_system_yields_system_role() {
        let v = serde_json::to_value(forge_one(&HistoryInput::System { text: "sys" })).unwrap();
        let back: ChatMessage = serde_json::from_value(v).unwrap();
        assert!(matches!(back.role, ChatRole::System));
    }

    #[test]
    fn forge_assistant_yields_assistant_role() {
        let v =
            serde_json::to_value(forge_one(&HistoryInput::Assistant { text: "hello" })).unwrap();
        let back: ChatMessage = serde_json::from_value(v).unwrap();
        assert!(matches!(back.role, ChatRole::Assistant));
    }

    #[test]
    fn forge_tool_call_round_trips_arguments() {
        let args = json!({"path": "a.txt"});
        let v = serde_json::to_value(forge_one(&HistoryInput::ToolCall {
            id: "call_1",
            name: "file_read",
            arguments: &args,
        }))
        .unwrap();
        let back: ChatMessage = serde_json::from_value(v).unwrap();
        assert!(matches!(back.role, ChatRole::Assistant));
    }

    #[test]
    fn forge_tool_result_yields_tool_role() {
        let v = serde_json::to_value(forge_one(&HistoryInput::ToolResult {
            call_id: "call_1",
            content: "ok",
            is_error: false,
        }))
        .unwrap();
        let back: ChatMessage = serde_json::from_value(v).unwrap();
        assert!(matches!(back.role, ChatRole::Tool));
    }

    #[test]
    fn remap_dedupes_recurring_call_ids_and_repairs_results() {
        // Two turns whose tool calls both carry genai's synthetic `call_0`
        // (the DeepSeek case). Replaying both in one request collides; remap
        // must make them unique and follow the rename onto each result.
        let args = json!({});
        let mut messages = vec![
            forge_one(&HistoryInput::ToolCall {
                id: "call_0",
                name: "first",
                arguments: &args,
            }),
            forge_one(&HistoryInput::ToolResult {
                call_id: "call_0",
                content: "r1",
                is_error: false,
            }),
            forge_one(&HistoryInput::ToolCall {
                id: "call_0",
                name: "second",
                arguments: &args,
            }),
            forge_one(&HistoryInput::ToolResult {
                call_id: "call_0",
                content: "r2",
                is_error: false,
            }),
        ];
        remap_tool_call_ids(&mut messages);

        let call_a = messages[0].content.tool_calls()[0].call_id.clone();
        let result_a = messages[1].content.tool_responses()[0].call_id.clone();
        let call_b = messages[2].content.tool_calls()[0].call_id.clone();
        let result_b = messages[3].content.tool_responses()[0].call_id.clone();

        // Each result still points at its own call...
        assert_eq!(call_a, result_a);
        assert_eq!(call_b, result_b);
        // ...but the two calls no longer collide.
        assert_ne!(call_a, call_b);
    }

    #[test]
    fn remap_is_deterministic_across_runs() {
        // Stable ids turn-over-turn are what keeps the prompt cache warm.
        let args = json!({});
        let build = || {
            vec![forge_one(&HistoryInput::ToolCall {
                id: "whatever",
                name: "t",
                arguments: &args,
            })]
        };
        let mut a = build();
        let mut b = build();
        remap_tool_call_ids(&mut a);
        remap_tool_call_ids(&mut b);
        assert_eq!(
            a[0].content.tool_calls()[0].call_id,
            b[0].content.tool_calls()[0].call_id,
        );
    }

    #[test]
    fn assistant_payload_round_trips_through_chat_message() {
        // The History event we emit must deserialise back into a typed
        // ChatMessage next turn — that's the bridge into the SDK request.
        let calls = vec![ToolCall {
            error: None,
            id: "call_1".into(),
            name: "edit".into(),
            arguments: json!({"path": "a.txt"}),
        }];
        let payload = build_assistant_payload("answer", &calls, "thought", &[])
            .expect("assistant payload must serialise");
        let msg: ChatMessage = serde_json::from_value(payload)
            .expect("History payload must deserialise as ChatMessage");
        assert!(matches!(msg.role, ChatRole::Assistant));
    }

    #[test]
    fn map_tool_call_parses_string_arguments_into_object() {
        // genai's OpenAI-shaped streamer hands us `Value::String(raw_json)`
        // for each ToolCallChunk; downstream consumers expect a parsed
        // Value::Object. map_tool_call must do that parse — otherwise
        // workflow tool handlers can't destructure call.arguments fields.
        let call = GenaiToolCall {
            call_id: "call_1".into(),
            fn_name: "file_read".into(),
            fn_arguments: Value::String("{\"path\":\"README.md\"}".into()),
            thought_signatures: None,
        };
        let mapped = map_tool_call(call);
        assert_eq!(mapped.arguments, json!({"path": "README.md"}));
    }

    #[test]
    fn map_tool_call_preserves_already_parsed_arguments() {
        let call = GenaiToolCall {
            call_id: "call_1".into(),
            fn_name: "file_read".into(),
            fn_arguments: json!({"path": "README.md"}),
            thought_signatures: None,
        };
        let mapped = map_tool_call(call);
        assert_eq!(mapped.arguments, json!({"path": "README.md"}));
    }

    #[test]
    fn map_tool_call_keeps_unparseable_string_as_string() {
        let call = GenaiToolCall {
            call_id: "call_1".into(),
            fn_name: "x".into(),
            fn_arguments: Value::String("not json".into()),
            thought_signatures: None,
        };
        let mapped = map_tool_call(call);
        assert_eq!(mapped.arguments, Value::String("not json".into()));
    }

    use crate::provider::Provider as _;
    use frances_config::{ConfigHandle, ConfigProvider, InMemoryProvider};
    use frances_models_llm::config::{AuthMethod, ProviderConfig};

    async fn handle_with(entries: &[(&str, bool)]) -> ConfigHandle {
        let mut p = InMemoryProvider::new();
        for (path, val) in entries {
            p = p.set(*path, *val);
        }
        let provider: std::sync::Arc<dyn ConfigProvider> = std::sync::Arc::new(p);
        ConfigHandle::build(vec![provider]).await.unwrap()
    }

    fn openrouter_provider_config() -> ProviderConfig {
        ProviderConfig {
            kind: "openrouter".into(),
            name: None,
            base_url: "https://openrouter.ai/api/v1".parse().unwrap(),
            auth: AuthMethod::Token {
                token: "stub".into(),
            },
            http_headers: Default::default(),
            query_params: Default::default(),
            supports_websockets: false,
            request_max_retries: 0,
            stream_max_retries: 0,
            stream_idle_timeout_ms: 1000,
        }
    }

    #[tokio::test]
    async fn qwen_quirks_for_reads_from_openrouter_binding() {
        let handle = handle_with(&[
            ("openrouter::models::qwen::qwen_quirks", true),
            ("openrouter::models::gpt::qwen_quirks", false),
        ])
        .await;
        let provider = Provider::new(openrouter_provider_config(), handle).unwrap();
        assert!(provider.qwen_quirks_for("qwen"));
        assert!(!provider.qwen_quirks_for("gpt"));
        assert!(!provider.qwen_quirks_for("unset-model"));
    }

    #[tokio::test]
    async fn qwen_quirks_for_returns_false_when_block_absent() {
        let handle = handle_with(&[]).await;
        let provider = Provider::new(openrouter_provider_config(), handle).unwrap();
        assert!(!provider.qwen_quirks_for("anything"));
    }

    #[tokio::test]
    async fn non_openrouter_provider_does_not_bind_openrouter_config() {
        let handle = handle_with(&[("openrouter::models::qwen::qwen_quirks", true)]).await;
        let mut anthropic_cfg = openrouter_provider_config();
        anthropic_cfg.kind = "anthropic".into();
        let provider = Provider::new(anthropic_cfg, handle).unwrap();
        assert!(provider.openrouter.is_none());
        assert!(!provider.qwen_quirks_for("qwen"));
    }

    #[test]
    fn map_usage_extracts_token_counts() {
        let u = genai::chat::Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            prompt_tokens_details: None,
            completion_tokens_details: None,
        };
        let mapped = map_usage(&u);
        assert_eq!(mapped.prompt_tokens, 10);
        assert_eq!(mapped.completion_tokens, 5);
        assert_eq!(mapped.total_tokens, 15);
    }
}
