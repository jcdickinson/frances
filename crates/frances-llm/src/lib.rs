//! LLM provider abstraction and the OpenAI-shaped wire implementation.

pub mod config;
pub mod provider;
pub mod providers;

pub use config::{
    AuthCommand, AuthMethod, ModelConfig, ProviderConfig, ResponsesModelExtras, WireApi,
};
pub use provider::{
    ChunkAbort, CompletionOutcome, ErasedError, ErasedProvider, ErasedResult, Provider,
    ProviderRequest, StreamEvent, ToolCall, ToolChoice, ToolDef, ToolFunction, Usage,
};
