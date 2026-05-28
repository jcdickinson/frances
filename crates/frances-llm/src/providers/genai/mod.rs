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
    /// populated when this provider's `kind` is `openrouter`. Other
    /// adapters that grow their own quirks would gain sibling fields here.
    openrouter: Option<ConfigBinding<OpenRouterConfig>>,
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
        }))
    }

    fn forge_history(&self, inputs: &[HistoryInput<'_>]) -> Vec<Value> {
        inputs
            .iter()
            .filter_map(|input| {
                let msg = forge_one(input)?;
                serde_json::to_value(&msg).ok()
            })
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

        let forged_new = self.forge_history(req.new_inputs);
        for payload in &forged_new {
            on_event(StreamEvent::History(payload.clone()))?;
        }

        // Bridge persisted history JSON back into typed `ChatMessage`s
        // for the request.
        let mut messages: Vec<ChatMessage> =
            Vec::with_capacity(req.history.len() + forged_new.len());
        for (i, v) in req.history.iter().chain(forged_new.iter()).enumerate() {
            let msg: ChatMessage = serde_json::from_value(v.clone())
                .map_err(|source| Error::DecodeHistory { index: i, source })?;
            messages.push(msg);
        }

        let chat_req = build_chat_request(messages, req.tools, req.tool_choice);
        let chat_options = build_chat_options(&plan, req.tool_choice);
        let model_id = plan.model.id.clone();

        let client = build_client(self.adapter, &plan)?;

        debug!(
            adapter = ?self.adapter,
            messages = chat_req.messages.len(),
            tools = req.tools.len(),
            base_url = %plan.base_url,
            model = %plan.model.id,
            "calling genai chat stream"
        );
        if let Ok(s) = serde_json::to_string(&chat_req) {
            // Cap is large enough that a typical multi-turn body fits
            // even with several tool messages — the previous 100-char
            // tail was useless for tool-round-trip debugging.
            trace!(body = %Truncated::<20000>::new(s), "chat request body");
        }

        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Error::Cancelled),
            res = client.exec_chat_stream(model_id.as_str(), chat_req, Some(&chat_options))
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
                    on_event(StreamEvent::TextDelta(chunk.content))?;
                }
                ChatStreamEvent::Chunk(_) => {}
                ChatStreamEvent::ReasoningChunk(chunk) if !chunk.content.is_empty() => {
                    // Reasoning rides its own channel — consumers (TUI,
                    // step-transcript summariser) treat it differently
                    // from response text. It's also retained verbatim for
                    // the assistant `reasoning_content` round-trip.
                    reasoning_text.push_str(&chunk.content);
                    on_event(StreamEvent::ReasoningDelta(chunk.content))?;
                }
                ChatStreamEvent::ReasoningChunk(_) => {}
                ChatStreamEvent::ThoughtSignatureChunk(chunk) if !chunk.content.is_empty() => {
                    thought_signatures.push(chunk.content);
                }
                ChatStreamEvent::ThoughtSignatureChunk(_) => {}
                ChatStreamEvent::ToolCallChunk(tc) => {
                    let mut call = map_tool_call(tc.tool_call);
                    if qwen_quirks {
                        frances_models_llm::tool_args::repair_qwen_quirks(&mut call, req.tools);
                    }
                    trace!(
                        call_id = %call.id,
                        name = %call.name,
                        arguments = %call.arguments,
                        "tool call from model",
                    );
                    on_event(StreamEvent::ToolCall(call.clone()))?;
                    tool_calls.push(call);
                    if let Some(cap) = max_tool_calls
                        && tool_calls.len() >= cap
                    {
                        let assistant = build_assistant_payload(
                            &text,
                            &tool_calls,
                            &reasoning_text,
                            &thought_signatures,
                        );
                        on_event(StreamEvent::History(assistant))?;
                        return Ok(CompletionOutcome { text, tool_calls });
                    }
                }
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
                }
            }
        }

        if let Some(u) = final_usage {
            on_event(StreamEvent::Usage(u))?;
        }

        let assistant =
            build_assistant_payload(&text, &tool_calls, &reasoning_text, &thought_signatures);
        on_event(StreamEvent::History(assistant))?;

        Ok(CompletionOutcome { text, tool_calls })
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

fn build_client(adapter: AdapterKind, plan: &RequestPlan) -> std::result::Result<Client, Error> {
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

/// Map one `HistoryInput` primitive to a `ChatMessage`. Returns `None`
/// to skip the entry (currently always returns Some — kept as an
/// Option so a future ChatMessage construction failure can degrade
/// gracefully rather than panic).
fn forge_one(input: &HistoryInput<'_>) -> Option<ChatMessage> {
    match input {
        HistoryInput::System { text } => Some(ChatMessage::system(text.to_string())),
        HistoryInput::User { text } => Some(ChatMessage::user(text.to_string())),
        HistoryInput::Assistant { text } => Some(ChatMessage::assistant(text.to_string())),
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
            Some(ChatMessage::assistant_tool_calls_with_thoughts(
                vec![call],
                Vec::new(),
            ))
        }
        HistoryInput::ToolResult {
            call_id,
            content,
            is_error: _,
        } => {
            let response = ToolResponse::new(call_id.to_string(), content.to_string());
            Some(ChatMessage::tool(MessageContent::from_parts(vec![
                ContentPart::ToolResponse(response),
            ])))
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
) -> Value {
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
    serde_json::to_value(&msg).unwrap_or_else(|err| {
        warn!(?err, "assistant ChatMessage failed to serialise");
        Value::Null
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn forge_user_round_trips_through_chat_message() {
        let v = forge_one(&HistoryInput::User { text: "hi" })
            .map(|m| serde_json::to_value(m).unwrap())
            .unwrap();
        let back: ChatMessage = serde_json::from_value(v).unwrap();
        assert!(matches!(back.role, ChatRole::User));
    }

    #[test]
    fn forge_system_yields_system_role() {
        let v = forge_one(&HistoryInput::System { text: "sys" })
            .map(|m| serde_json::to_value(m).unwrap())
            .unwrap();
        let back: ChatMessage = serde_json::from_value(v).unwrap();
        assert!(matches!(back.role, ChatRole::System));
    }

    #[test]
    fn forge_assistant_yields_assistant_role() {
        let v = forge_one(&HistoryInput::Assistant { text: "hello" })
            .map(|m| serde_json::to_value(m).unwrap())
            .unwrap();
        let back: ChatMessage = serde_json::from_value(v).unwrap();
        assert!(matches!(back.role, ChatRole::Assistant));
    }

    #[test]
    fn forge_tool_call_round_trips_arguments() {
        let args = json!({"path": "a.txt"});
        let v = forge_one(&HistoryInput::ToolCall {
            id: "call_1",
            name: "file_read",
            arguments: &args,
        })
        .map(|m| serde_json::to_value(m).unwrap())
        .unwrap();
        let back: ChatMessage = serde_json::from_value(v).unwrap();
        assert!(matches!(back.role, ChatRole::Assistant));
    }

    #[test]
    fn forge_tool_result_yields_tool_role() {
        let v = forge_one(&HistoryInput::ToolResult {
            call_id: "call_1",
            content: "ok",
            is_error: false,
        })
        .map(|m| serde_json::to_value(m).unwrap())
        .unwrap();
        let back: ChatMessage = serde_json::from_value(v).unwrap();
        assert!(matches!(back.role, ChatRole::Tool));
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
        let payload = build_assistant_payload("answer", &calls, "thought", &[]);
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
