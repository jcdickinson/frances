//! One-shot completion request + the forced-tool "enforce" vocabulary.
//!
//! Lives in `frances-models-llm` (not `frances-llm`) so the
//! [`ChatSessionManager`](super::ChatSessionManager) trait can take
//! `CompleteRequest` and expose `complete` / `complete_enforced` to
//! trait-only callers (workflow code, the JS `complete` export).

use std::collections::HashMap;
use std::ffi::OsString;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::chat::error::ChatError;
use crate::wire::{CompletionOutcome, HistoryInput, ToolChoice, ToolDef};

/// Inputs to [`ChatSessionManager::complete`](super::ChatSessionManager::complete).
/// Bundled so the call site reads as `chat.complete(CompleteRequest { … })`
/// instead of a wall of positional args.
pub struct CompleteRequest<'a> {
    /// Model-intent names to walk; first hit wins, default fallback.
    pub intents: &'a [&'a str],
    /// Token-cache scope id. For classifier-style calls during a chat
    /// turn, pass the parent chat session's id.
    pub session_id: &'a str,
    pub env: &'a HashMap<OsString, OsString>,
    pub history: &'a [Value],
    pub new_inputs: &'a [HistoryInput<'a>],
    pub tools: &'a [ToolDef],
    pub tool_choice: Option<&'a ToolChoice>,
    /// Firing cancels the in-flight provider stream. Pass
    /// `CancellationToken::new()` (never fires) when the caller has no
    /// upstream abort source.
    pub cancel: CancellationToken,
    /// Optional cap on tool calls.
    pub max_tool_calls: Option<usize>,
}

/// The demanding subset of [`ToolChoice`] for
/// [`ChatSessionManager::complete_enforced`](super::ChatSessionManager::complete_enforced).
/// Excludes `Auto`/`None` so "enforce nothing" can't be requested.
#[derive(Debug, Clone)]
pub enum Demand {
    /// Any tool call satisfies.
    Required,
    /// Only a call to the named tool satisfies.
    Function(String),
}

impl Demand {
    pub(crate) fn to_tool_choice(&self) -> ToolChoice {
        match self {
            Demand::Required => ToolChoice::Required,
            Demand::Function(name) => ToolChoice::Function(name.clone()),
        }
    }

    /// Whether `outcome` honours the demand.
    pub fn satisfied_by(&self, outcome: &CompletionOutcome) -> bool {
        match self {
            Demand::Required => !outcome.tool_calls.is_empty(),
            Demand::Function(name) => outcome.tool_calls.iter().any(|c| &c.name == name),
        }
    }

    pub(crate) fn scold(&self) -> String {
        match self {
            Demand::Required => {
                "Your last response didn't call any tool. Respond with exactly one tool call."
                    .to_owned()
            }
            Demand::Function(name) => format!(
                "Your last response didn't call the required `{name}` tool. \
                 Respond with exactly one call to `{name}`."
            ),
        }
    }

    pub(crate) fn unsatisfied_detail(&self) -> String {
        match self {
            Demand::Required => "expected at least one tool call".to_owned(),
            Demand::Function(name) => format!("expected a call to `{name}`"),
        }
    }
}

/// Failure modes of
/// [`ChatSessionManager::complete_enforced`](super::ChatSessionManager::complete_enforced).
#[derive(Debug, thiserror::Error)]
pub enum EnforceError {
    /// The underlying completion failed (transport / provider / cancel).
    #[error(transparent)]
    Provider(#[from] ChatError),
    /// The model never produced a satisfying tool call within the retry
    /// budget.
    #[error("forced tool not satisfied: {detail}")]
    Unsatisfied { detail: String },
}
