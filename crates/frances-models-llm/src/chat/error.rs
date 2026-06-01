use thiserror::Error;

use crate::ErasedError;

use super::types::ChatSessionId;

/// Storage-agnostic errors raised by `HistoryStore` impls (in
/// `frances-llm`). Backend failures (turso, in-memory, etc.) get wrapped
/// via the `Backend` variant.
#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("encode {what}: {source}")]
    Encode {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("decode {what}: {source}")]
    Decode {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("chat_session {0} not found")]
    ChatSessionNotFound(ChatSessionId),
    #[error("backend: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// Errors raised by `ChatSession` and `ChatSessionManager` operations.
#[derive(Debug, Error)]
pub enum ChatError {
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("model_providers.{0} not available (no config or factory missing)")]
    ProviderUnavailable(String),
    #[error("provider {provider_id}: {source}")]
    Provider {
        provider_id: String,
        #[source]
        source: ErasedError,
    },
    /// The caller fired the `CancellationToken` passed to `run`/`complete`
    /// before the provider stream finished.
    #[error("cancelled")]
    Cancelled,
}
