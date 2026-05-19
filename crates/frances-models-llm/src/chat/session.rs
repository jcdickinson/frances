//! Workflow-facing traits for chat sessions.
//!
//! The concrete `ChatSession` / `ChatSessionManager` structs live in
//! `frances-llm`. Workflow code uses these traits exclusively, so it can
//! depend only on `frances-models-llm` (no `Provider` trait, no HTTP).

use std::collections::HashMap;
use std::ffi::OsString;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::wire::{CompletionOutcome, StreamEvent, ToolChoice, ToolDef};

use super::builder::ChatSessionBuilder;
use super::error::ChatError;
use super::types::{ChatSessionId, OwnedHistoryInput};

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
}

#[async_trait]
pub trait ChatSessionManager: Clone + Send + Sync + 'static {
    type Session: ChatSession;

    /// Mint a fresh in-memory session. The DB `chat_sessions` row is
    /// written lazily on first `run`, not here.
    fn create(&self, builder: ChatSessionBuilder) -> Self::Session;

    /// Load a previously-persisted session by id.
    async fn load(&self, id: ChatSessionId) -> Result<Self::Session, ChatError>;
}
