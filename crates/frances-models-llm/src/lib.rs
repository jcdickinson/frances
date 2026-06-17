//! Workflow-facing types + traits for chat sessions and LLM providers.

pub mod chat;
pub mod completion;
pub mod config;
pub mod erased;
pub mod history;
pub mod tool;
pub mod tool_args;

pub use chat::{
    ChatError, ChatSession, ChatSessionBuilder, ChatSessionId, ChatSessionManager, ChatSessionRow,
    HistoryError,
};
pub use completion::{CompletionOutcome, StreamEvent, ToolCall, ToolCallError, Usage};
pub use config::{
    AuthCommand, AuthMethod, ModelConfig, OpenRouterConfig, OpenRouterModelConfig, ProviderConfig,
};
pub use erased::{ChunkAbort, ErasedError, ErasedResult};
pub use history::{BatchRow, HistoryBatch, HistoryInput, OwnedHistoryInput};
pub use tool::{ToolChoice, ToolDef, ToolFunction};
