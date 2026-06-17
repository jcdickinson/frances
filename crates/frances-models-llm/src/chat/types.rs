use crate::chat::builder::ModelIntents;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChatSessionId(pub i64);

impl std::fmt::Display for ChatSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Row-shape of a `chat_sessions` entry, returned from `load_chat_session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatSessionRow {
    pub id: ChatSessionId,
    /// Opaque UUID.
    pub session_id: String,
    /// Ordered list of `models::<intent>` config keys the session walks
    /// when resolving a model for the next call.
    pub model_intents: ModelIntents,
}
