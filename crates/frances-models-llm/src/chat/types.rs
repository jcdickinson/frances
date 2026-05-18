use crate::chat::builder::ModelIntents;
use crate::wire::HistoryInput;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// A primitive row read back from storage; mirrors [`HistoryInput`] but
/// owns its strings so it can outlive the SQL row buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedHistoryInput {
    System {
        text: String,
    },
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

impl OwnedHistoryInput {
    pub fn as_borrowed(&self) -> HistoryInput<'_> {
        match self {
            Self::System { text } => HistoryInput::System { text },
            Self::User { text } => HistoryInput::User { text },
            Self::Assistant { text } => HistoryInput::Assistant { text },
            Self::ToolCall {
                id,
                name,
                arguments,
            } => HistoryInput::ToolCall {
                id,
                name,
                arguments,
            },
            Self::ToolResult {
                call_id,
                content,
                is_error,
            } => HistoryInput::ToolResult {
                call_id,
                content,
                is_error: *is_error,
            },
        }
    }
}
