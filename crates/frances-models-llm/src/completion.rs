//! The model's output: streaming [`StreamEvent`]s during a turn and the
//! final [`CompletionOutcome`] after it, plus the parsed [`ToolCall`]s (each
//! optionally flagged with a [`ToolCallError`]) and [`Usage`] accounting.
//! Assembled in-memory by the provider layer from the `genai` stream — not
//! serialized.

use serde_json::Value;

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
    /// A fragment of model reasoning / chain-of-thought. Kept on a separate
    /// channel from `TextDelta` so consumers can render it in its own block
    /// and keep it out of chat-history-derived transcripts.
    ReasoningDelta(String),
    /// A completed tool call.
    ToolCall(ToolCall),
    /// A wire-shape JSON to be persisted for use as future history.
    History(Value),
    /// Final-frame token accounting. May be emitted once at the end of the
    /// stream; not all wires populate it.
    Usage(Usage),
}

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

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON-shaped args the model supplied for this call.
    pub arguments: Value,
    /// `Some` when the chat layer validated `arguments` against the called
    /// tool's declared schema and they didn't match. The call was still
    /// emitted by the model (so it stays in `tool_calls` and gets persisted);
    /// dispatch turns it into an error tool result the model can correct.
    pub error: Option<ToolCallError>,
}

/// Why a [`ToolCall`]'s arguments failed schema validation, plus the schema
/// they were expected to satisfy. Enough for the caller to build a corrective
/// error result the model can self-correct against.
#[derive(Debug, Clone)]
pub struct ToolCallError {
    /// The JSON schema the arguments were checked against.
    pub expected_schema: Value,
    pub message: String,
}

/// Token-usage report. Universal shape; `cached_input_tokens` mirrors
/// OpenAI's `prompt_tokens_details.cached_tokens` for the wires that
/// surface it.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cached_input_tokens: u32,
}
