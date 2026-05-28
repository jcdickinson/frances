//! LLM provider implementations + chat session machinery.
//!
//! Value types + workflow-facing traits live in `frances-models-llm`.
//! This crate holds the concrete `Provider` impl(s), the
//! `ProviderCache`, and the concrete `ChatSession`/`ChatSessionManager`
//! that workflow and runtime use.

pub mod chat;
pub mod provider;
pub mod provider_cache;
pub mod providers;

#[cfg(any(test, feature = "test-util"))]
pub mod test_util;

pub use chat::{
    ChatManagerDeps, ChatSession, ChatSessionManager, CompleteRequest, Demand, EnforceError,
    HistoryStore,
};
pub use provider::{ErasedProvider, Provider, ProviderRequest};
pub use provider_cache::{ProviderCache, ProviderCacheError};

// Re-export the model-side surface so existing imports
// (`frances_llm::HistoryInput`, etc.) keep working transitionally
// where it's still convenient. New code should import from
// `frances_models_llm::*` directly.
pub use frances_models_llm::config::{
    AuthCommand, AuthMethod, ModelConfig, OpenRouterConfig, OpenRouterModelConfig, ProviderConfig,
};
pub use frances_models_llm::{
    ChunkAbort, CompletionOutcome, ErasedError, ErasedResult, HistoryInput, StreamEvent, ToolCall,
    ToolChoice, ToolDef, ToolFunction, Usage,
};
