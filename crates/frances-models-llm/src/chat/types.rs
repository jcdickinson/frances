use crate::chat::builder::ModelIntents;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RowId(pub i64);

impl std::fmt::Display for RowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RowSeq(pub i64);

/// Opaque marker captured by [`ChatSession::checkpoint`] and consumed by
/// [`ChatSession::rollback`] to discard everything appended since.
/// `persisted` is the high-water persisted row id (`None` for ephemeral
/// sessions); `pending_len` is the count of un-drained in-memory inputs
/// at checkpoint time.
///
/// [`ChatSession::checkpoint`]: crate::chat::ChatSession::checkpoint
/// [`ChatSession::rollback`]: crate::chat::ChatSession::rollback
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatCheckpoint {
    pub persisted: Option<RowId>,
    pub pending_len: usize,
}

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
    /// Opaque UUID; threaded through provider requests as a cache-scoping
    /// hint.
    pub session_id: String,
    /// Ordered list of `models::<intent>` config keys the session walks
    /// when resolving a model for the next call.
    pub model_intents: ModelIntents,
}
