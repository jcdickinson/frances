//! Workflow-facing traits for chat sessions.
//!
//! The concrete `ChatSession` / `ChatSessionManager` structs live in
//! `frances-llm`. Workflow code uses these traits exclusively, so it can
//! depend only on `frances-models-llm` (no `Provider` trait, no HTTP).

use std::collections::HashMap;
use std::ffi::OsString;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::wire::{CompletionOutcome, HistoryInput, StreamEvent, ToolChoice, ToolDef};

use super::builder::ChatSessionBuilder;
use super::complete::{CompleteRequest, Demand, EnforceError};
use super::error::ChatError;
use super::types::{ChatCheckpoint, ChatSessionId, OwnedHistoryInput};

#[async_trait]
pub trait ChatSession: Clone + Send + Sync + 'static {
    /// Append a pending input that will be drained by the next `run` call.
    /// Sync — DB persistence happens inside `run`.
    fn push(&self, input: OwnedHistoryInput);

    /// Drive one provider call: drain pending, write primitives, load
    /// history, stream, persist `History` payloads + the assistant reply.
    ///
    /// Firing `cancel` aborts the in-flight provider request; the call
    /// returns `Err(ChatError::Cancelled)` and the underlying HTTP
    /// connection is dropped so the provider stops generating.
    ///
    /// `max_tool_calls` caps how many tool calls the provider will
    /// retain — see `ProviderRequest::max_tool_calls` in `frances-llm`.
    /// `None` is unbounded.
    async fn run(
        &self,
        env: HashMap<OsString, OsString>,
        tools: Vec<ToolDef>,
        tool_choice: Option<ToolChoice>,
        cancel: CancellationToken,
        max_tool_calls: Option<usize>,
        on_event: Box<dyn FnMut(StreamEvent) -> Result<(), ChatError> + Send>,
    ) -> Result<CompletionOutcome, ChatError>;

    /// Snapshot the session's history position. Pair with
    /// [`rollback`](Self::rollback) to discard everything appended
    /// since — used by workflows to drop a partial round (e.g. an
    /// assistant turn whose tool calls never got results) when an
    /// interjection/interrupt aborts mid-flight.
    async fn checkpoint(&self) -> Result<ChatCheckpoint, ChatError>;

    /// Roll history back to a [`ChatCheckpoint`]: truncate the
    /// un-drained pending queue and delete persisted rows appended
    /// after the marker.
    async fn rollback(&self, checkpoint: ChatCheckpoint) -> Result<(), ChatError>;
}

#[async_trait]
pub trait ChatSessionManager: Clone + Send + Sync + 'static {
    type Session: ChatSession;

    /// Mint a fresh in-memory session. The DB `chat_sessions` row is
    /// written lazily on first `run`, not here.
    fn create(&self, builder: ChatSessionBuilder) -> Self::Session;

    /// Load a previously-persisted session by id.
    async fn load(&self, id: ChatSessionId) -> Result<Self::Session, ChatError>;

    /// One-shot, non-persisted call: resolve a model by walking
    /// `req.intents`, then call the provider with `req.history` +
    /// `req.new_inputs` verbatim. Nothing is read from or written to a
    /// history store. Provider-specific, so it's abstract.
    async fn complete(&self, req: CompleteRequest<'_>) -> Result<CompletionOutcome, ChatError>;

    /// Like [`complete`](Self::complete), but *demands* a tool call and
    /// distrusts the provider: after each round it checks the outcome
    /// against `demand`, and on a miss appends a generic structural
    /// scold and retries (up to `retries` times). The demand drives
    /// `tool_choice` — any `tool_choice` on `req` is ignored. Generic
    /// over `complete`, so it's a provided default.
    async fn complete_enforced(
        &self,
        req: CompleteRequest<'_>,
        demand: Demand,
        retries: u8,
    ) -> Result<CompletionOutcome, EnforceError> {
        let tool_choice = demand.to_tool_choice();
        let scold = demand.scold();
        // Own the inputs so we can grow them with scolds across rounds;
        // covariance lets the borrowed clones sit alongside `&scold`.
        let mut inputs: Vec<HistoryInput> = req.new_inputs.to_vec();
        let mut attempts_left = retries;
        loop {
            let round = CompleteRequest {
                intents: req.intents,
                session_id: req.session_id,
                env: req.env,
                history: req.history,
                new_inputs: &inputs,
                tools: req.tools,
                tool_choice: Some(&tool_choice),
                cancel: req.cancel.clone(),
                max_tool_calls: req.max_tool_calls,
            };
            let outcome = self.complete(round).await?;
            if demand.satisfied_by(&outcome) {
                return Ok(outcome);
            }
            if attempts_left == 0 {
                return Err(EnforceError::Unsatisfied {
                    detail: demand.unsatisfied_detail(),
                });
            }
            inputs.push(HistoryInput::User { text: &scold });
            attempts_left -= 1;
        }
    }
}
