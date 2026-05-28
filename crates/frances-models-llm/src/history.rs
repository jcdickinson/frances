//! Conversation content shared with provider implementations: the
//! borrowed [`HistoryInput`] the provider forges into its own wire shape,
//! and the owned [`OwnedHistoryInput`] mirror used to carry persisted rows
//! across `.await` points and storage boundaries.

use serde_json::Value;

/// Primitive content the provider may need to forge into wire shape — both
/// inline during `stream` (for the just-arrived turn delta) and in batch
/// during a swap-time `forge_history` call (rebuilding the cache from
/// every primitive row in the conversation).
#[derive(Debug, Clone)]
pub enum HistoryInput<'a> {
    System {
        text: &'a str,
    },
    User {
        text: &'a str,
    },
    Assistant {
        text: &'a str,
    },
    ToolCall {
        id: &'a str,
        name: &'a str,
        arguments: &'a Value,
    },
    ToolResult {
        call_id: &'a str,
        content: &'a str,
        is_error: bool,
    },
}

/// Owned mirror of [`HistoryInput`] — owns its strings so it can outlive a
/// SQL row buffer or sit in a queue across `.await`. Round-trips with the
/// borrowed form via [`as_borrowed`](Self::as_borrowed) /
/// [`from_borrowed`](Self::from_borrowed).
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
    /// Owning copy of a borrowed [`HistoryInput`] — the inverse of
    /// [`as_borrowed`](Self::as_borrowed). Lets callers that need to grow
    /// an input list across rounds (e.g. `complete_enforced`'s scold loop)
    /// keep everything owned.
    pub fn from_borrowed(input: &HistoryInput<'_>) -> Self {
        match *input {
            HistoryInput::System { text } => Self::System {
                text: text.to_owned(),
            },
            HistoryInput::User { text } => Self::User {
                text: text.to_owned(),
            },
            HistoryInput::Assistant { text } => Self::Assistant {
                text: text.to_owned(),
            },
            HistoryInput::ToolCall {
                id,
                name,
                arguments,
            } => Self::ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments: arguments.clone(),
            },
            HistoryInput::ToolResult {
                call_id,
                content,
                is_error,
            } => Self::ToolResult {
                call_id: call_id.to_owned(),
                content: content.to_owned(),
                is_error,
            },
        }
    }

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
