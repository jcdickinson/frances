//! Workflow-facing types + traits for chat sessions and LLM providers.
//!
//! `frances-llm` holds the actual provider implementations and the
//! concrete `ChatSession`/`ChatSessionManager` structs. This crate is
//! pure data + traits so consumers (notably `frances-workflow`) don't
//! pull in HTTP / SSE machinery.

pub mod chat;
pub mod config;
pub mod wire;

pub use chat::{
    ChatError, ChatSession, ChatSessionBuilder, ChatSessionId, ChatSessionManager, ChatSessionRow,
    HistoryError, OwnedHistoryInput, RowId, RowSeq,
};
pub use config::{AuthCommand, AuthMethod, GenAIExtras, ModelConfig, ProviderConfig};
pub use wire::{
    ChunkAbort, CompletionOutcome, ErasedError, ErasedResult, HistoryInput, StreamEvent, ToolCall,
    ToolChoice, ToolDef, ToolFunction, Usage,
};
