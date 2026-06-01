//! One-shot completion request + the forced-tool "enforce" vocabulary.

use std::collections::HashMap;
use std::ffi::OsString;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::chat::error::ChatError;
use crate::{CompletionOutcome, HistoryInput, ToolCall, ToolChoice, ToolDef};

/// Inputs to [`ChatSessionManager::complete`](super::ChatSessionManager::complete).
pub struct CompleteRequest<'a> {
    /// Model-intent names to walk; first hit wins, default fallback.
    pub intents: &'a [&'a str],
    /// Token-cache scope id.
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

    /// Whether a call matches what this demand asks for, ignoring validity.
    fn matches(&self, call: &ToolCall) -> bool {
        match self {
            Demand::Required => true,
            Demand::Function(name) => &call.name == name,
        }
    }

    /// Whether `outcome` honours the demand. A call whose arguments failed
    /// schema validation ([`ToolCall::error`]) doesn't count — the model
    /// asked for the tool but botched the arguments, so it isn't satisfied.
    pub fn satisfied_by(&self, outcome: &CompletionOutcome) -> bool {
        outcome
            .tool_calls
            .iter()
            .any(|c| c.error.is_none() && self.matches(c))
    }

    /// Validation error of a demanded call the model *did* emit but with bad
    /// arguments, if any — fed back into the scold so it can self-correct.
    fn demanded_error<'a>(&self, outcome: &'a CompletionOutcome) -> Option<&'a str> {
        outcome
            .tool_calls
            .iter()
            .filter(|c| self.matches(c))
            .find_map(|c| c.error.as_ref())
            .map(|e| e.message.as_str())
    }

    pub(crate) fn scold(&self, outcome: &CompletionOutcome) -> String {
        let base = match self {
            Demand::Required => {
                "Your last response didn't call any tool. Respond with exactly one tool call."
                    .to_owned()
            }
            Demand::Function(name) => format!(
                "Your last response didn't call the required `{name}` tool. \
                 Respond with exactly one call to `{name}`."
            ),
        };
        match self.demanded_error(outcome) {
            Some(err) => format!("{base} The previous arguments were invalid: {err}"),
            None => base,
        }
    }

    pub(crate) fn unsatisfied_detail(&self, outcome: &CompletionOutcome) -> String {
        let base = match self {
            Demand::Required => "expected at least one tool call".to_owned(),
            Demand::Function(name) => format!("expected a call to `{name}`"),
        };
        match self.demanded_error(outcome) {
            Some(err) => format!("{base} (last call had invalid arguments: {err})"),
            None => base,
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
