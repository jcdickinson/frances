//! Conversation content shared with provider implementations: the
//! borrowed [`HistoryInput`] the provider forges into its own wire shape,
//! and the owned [`OwnedHistoryInput`] mirror used to carry persisted rows
//! across `.await` points and storage boundaries.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chat::HistoryError;

/// Hard ceiling for one tool result entering conversation history. Tools
/// should apply their own semantic limits first; this is the last-resort
/// guard against an overlooked unbounded string.
pub const TOOL_RESULT_BYTE_CAP: usize = 16 * 1024;

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
///
/// The `#[serde(tag = "kind")]` shape is the on-disk primitive payload:
/// one `to_value`/`from_value` round-trips every variant, and
/// [`kind`](Self::kind) yields the same tag for the indexable `type`
/// column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
    /// [`as_borrowed`](Self::as_borrowed).
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

    /// The serde tag for this variant, mirrored into the indexable `type` column.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::System { .. } => "system",
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
        }
    }

    /// Bound tool content before it is persisted or sent to a provider.
    /// Other message kinds are intentionally untouched.
    pub fn truncate_tool_result(&mut self) {
        let Self::ToolResult { content, .. } = self else {
            return;
        };
        if content.len() <= TOOL_RESULT_BYTE_CAP {
            return;
        }

        let original_len = content.len();
        let marker = format!(
            "\n… tool result truncated from {original_len} bytes at the \
             {TOOL_RESULT_BYTE_CAP}-byte history limit …"
        );
        let mut end = TOOL_RESULT_BYTE_CAP.saturating_sub(marker.len());
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
        content.push_str(&marker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_under_cap_is_unchanged() {
        let mut input = OwnedHistoryInput::ToolResult {
            call_id: "c1".to_owned(),
            content: "small result".to_owned(),
            is_error: false,
        };

        input.truncate_tool_result();

        assert!(matches!(
            input,
            OwnedHistoryInput::ToolResult { content, .. } if content == "small result"
        ));
    }

    #[test]
    fn tool_result_cap_preserves_utf8_and_reports_truncation() {
        let mut input = OwnedHistoryInput::ToolResult {
            call_id: "c1".to_owned(),
            content: "🦀".repeat(TOOL_RESULT_BYTE_CAP),
            is_error: false,
        };

        input.truncate_tool_result();

        let OwnedHistoryInput::ToolResult { content, .. } = input else {
            panic!("expected tool result");
        };
        assert!(content.len() <= TOOL_RESULT_BYTE_CAP);
        assert!(content.contains("tool result truncated from"));
        assert!(content.ends_with("history limit …"));
    }
}

/// One row queued for a [`HistoryBatch`] flush.
pub enum BatchRow {
    /// A conversation primitive. `ty` is [`OwnedHistoryInput::kind`];
    /// `json` is the serialized `OwnedHistoryInput`.
    Primitive { ty: &'static str, json: String },
    /// A provider-forged wire payload, tagged with the provider kind and
    /// id that produced it.
    History {
        json: String,
        kind: String,
        provider_id: String,
    },
}

/// In-memory accumulator for a turn's history writes. Each `primitive` /
/// `history` call serializes eagerly and appends a [`BatchRow`]; the store
/// flushes the whole batch under one transaction with a single sequence read.
#[derive(Default)]
pub struct HistoryBatch {
    pub rows: Vec<BatchRow>,
}

impl HistoryBatch {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Queue a conversation primitive.
    pub fn primitive(&mut self, input: &OwnedHistoryInput) -> Result<(), HistoryError> {
        let json = serde_json::to_string(input).map_err(|source| HistoryError::Encode {
            what: "primitive",
            source,
        })?;
        self.rows.push(BatchRow::Primitive {
            ty: input.kind(),
            json,
        });
        Ok(())
    }

    /// Queue a provider-forged history payload.
    pub fn history(
        &mut self,
        payload: &Value,
        kind: &str,
        provider_id: &str,
    ) -> Result<(), HistoryError> {
        let json = serde_json::to_string(payload).map_err(|source| HistoryError::Encode {
            what: "history payload",
            source,
        })?;
        self.rows.push(BatchRow::History {
            json,
            kind: kind.to_owned(),
            provider_id: provider_id.to_owned(),
        });
        Ok(())
    }
}
