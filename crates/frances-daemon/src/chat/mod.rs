mod builder;
mod manager;
mod session;

use thiserror::Error;

pub use builder::ChatSessionBuilder;
pub use manager::{ChatSessionManager, CompleteRequest};
pub use session::ChatSession;

#[derive(Debug, Error)]
pub enum ChatError {
    #[error("model_providers.{0} not available (no config or factory missing)")]
    ProviderUnavailable(String),
    #[error("provider {provider_id}: {source}")]
    Provider {
        provider_id: String,
        #[source]
        source: frances_llm::ErasedError,
    },
}
