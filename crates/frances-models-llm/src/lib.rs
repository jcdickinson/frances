//! Workflow-facing types + traits for chat sessions and LLM providers.
//!
//! `frances-llm` holds the actual provider implementations and the
//! concrete `ChatSession`/`ChatSessionManager` structs. This crate is
//! pure data + traits so consumers (notably `frances-workflow`) don't
//! pull in HTTP / SSE machinery.

pub mod chat;
pub mod completion;
pub mod config;
pub mod erased;
pub mod history;
pub mod tool;
pub mod tool_args;

pub use chat::{
    ChatError, ChatSession, ChatSessionBuilder, ChatSessionId, ChatSessionManager, ChatSessionRow,
    HistoryError, RowId, RowSeq,
};
pub use completion::{CompletionOutcome, StreamEvent, ToolCall, ToolCallError, Usage};
pub use config::{
    AuthCommand, AuthMethod, ModelConfig, OpenRouterConfig, OpenRouterModelConfig, ProviderConfig,
};
pub use erased::{ChunkAbort, ErasedError, ErasedResult};
pub use history::{HistoryInput, OwnedHistoryInput};
pub use tool::{ToolChoice, ToolDef, ToolFunction};
