//! LLM-backed auto-approver for permission gates flagged with
//! `allow_auto = true`.
//!
//! When a workflow opts a gate into auto, the runtime's emit loop
//! calls [`judge`] before forwarding the request to the TUI. The
//! judge walks the model-intent fallback `["auto", "referee", "cheap"]`
//! and forces a single `decide` tool whose `verdict` is the decision.
//! Compliance is enforced by [`ChatSessionManager::complete_enforced`]
//! (force the tool, distrust the provider, one bounded scold), so the
//! caller no longer hand-rolls a retry loop.
//!
//! On `Approve` the runtime resolves the permission's oneshot
//! directly and the TUI never sees the prompt. On `Reject` or
//! `Indeterminate` the runtime falls through to the user — the
//! judge is an opt-in fast path, not an authority on denial.
//!
//! The judge does not see history. It does not see the structured
//! `tool_call`. It sees only the workflow-composed prompt, since
//! that prompt is already the canonical rendering of "what's being
//! requested". Feeding the structured call in too would
//! double-render.

use std::sync::Arc;
use std::sync::LazyLock;

use frances_llm::{CompleteRequest, Demand};
// The trait, in scope (as `_`) so `complete_enforced` resolves on the
// concrete `runtime.chat` manager.
use frances_models_llm::chat::ChatSessionManager as _;
use frances_models_llm::wire::{HistoryInput, ToolCall, ToolDef, ToolFunction};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::events::PermissionRequest;

use super::SessionRuntime;

/// Model-intent fallback walked by the judge. Resolved through the
/// chat manager's standard intent lookup; a missing intent falls
/// through to the next one and ultimately to `models::default`.
const INTENTS: &[&str] = &["auto", "referee", "cheap"];

const SYSTEM_PROMPT: &str = include_str!("auto_judge/system_prompt.md");

/// The single forced tool. `verdict` carries the decision. The schema is
/// kept strict-compatible (`additionalProperties: false`, both `required`)
/// so the provider gets OpenAI strict mode automatically; host-side
/// validation backs it up where the provider ignores strict.
static DECIDE_TOOL: LazyLock<ToolDef> = LazyLock::new(|| {
    ToolDef::Function(ToolFunction {
        name: "decide".into(),
        description: "Decide whether to auto-approve the proposed action. Call this exactly once."
            .into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "verdict": {
                    "type": "string",
                    "enum": ["approve", "reject"],
                    "description": "`approve` if the action is clearly fine for this project, otherwise `reject`.",
                },
                "reason": {
                    "type": "string",
                    "description": "One-sentence justification.",
                },
            },
            "required": ["verdict", "reason"],
        }),
    })
});

#[derive(Debug)]
pub(crate) enum JudgeOutcome {
    Approve {
        reason: String,
    },
    Reject {
        reason: String,
    },
    /// Judge couldn't reach a decision. Caller falls through to the
    /// user, same as `Reject`, but logs at a higher level.
    Indeterminate {
        reason: String,
    },
}

const DEFAULT_REASON: &str = "(no reason given)";

/// Ask the configured judge model whether to auto-approve `request`.
/// `complete_enforced` forces the `decide` tool and scolds once on a
/// miss; if it still can't get a call, that's `Indeterminate`.
pub(crate) async fn judge(
    runtime: &Arc<SessionRuntime>,
    request: &PermissionRequest,
) -> JudgeOutcome {
    let env = runtime.invocation.lock().process.env.clone();
    let session_id = format!("auto-judge:{}", uuid::Uuid::new_v4());
    let tools = [DECIDE_TOOL.clone()];
    let inputs: Vec<HistoryInput<'_>> = vec![
        HistoryInput::System {
            text: SYSTEM_PROMPT,
        },
        HistoryInput::User {
            text: request.prompt.as_str(),
        },
    ];

    let req = CompleteRequest {
        intents: INTENTS,
        session_id: &session_id,
        env: &env,
        history: &[],
        new_inputs: &inputs,
        tools: &tools,
        // `complete_enforced` drives tool_choice from the demand.
        tool_choice: None,
        cancel: CancellationToken::new(),
        max_tool_calls: Some(1),
    };

    match runtime
        .chat
        .complete_enforced(req, Demand::Function("decide".into()), 1)
        .await
    {
        Ok(outcome) => parse_decision(&outcome.tool_calls),
        Err(error) => {
            warn!(%error, "auto-judge: complete_enforced failed");
            JudgeOutcome::Indeterminate {
                reason: error.to_string(),
            }
        }
    }
}

/// Pure parser over the enforced `decide` call. `verdict` selects the
/// outcome; `reason` is `arguments.reason` if present and a string.
fn parse_decision(calls: &[ToolCall]) -> JudgeOutcome {
    let Some(call) = calls.iter().find(|c| c.name == "decide") else {
        // `complete_enforced` guarantees a `decide` call on `Ok`, so this
        // is defensive only.
        return JudgeOutcome::Indeterminate {
            reason: "no `decide` tool call".into(),
        };
    };
    let reason = call
        .arguments
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_REASON.to_owned());
    match call.arguments.get("verdict").and_then(|v| v.as_str()) {
        Some("approve") => JudgeOutcome::Approve { reason },
        Some("reject") => JudgeOutcome::Reject { reason },
        other => JudgeOutcome::Indeterminate {
            reason: format!("unexpected verdict {other:?}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decide(args: serde_json::Value) -> ToolCall {
        ToolCall {
            error: None,
            id: "1".into(),
            name: "decide".into(),
            arguments: args,
        }
    }

    #[test]
    fn parses_approve() {
        let calls = [decide(
            json!({ "verdict": "approve", "reason": "looks fine" }),
        )];
        match parse_decision(&calls) {
            JudgeOutcome::Approve { reason } => assert_eq!(reason, "looks fine"),
            other => panic!("expected Approve, got {other:?}"),
        }
    }

    #[test]
    fn parses_reject() {
        let calls = [decide(
            json!({ "verdict": "reject", "reason": "rm -rf is sketchy" }),
        )];
        match parse_decision(&calls) {
            JudgeOutcome::Reject { reason } => assert_eq!(reason, "rm -rf is sketchy"),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn missing_reason_defaults() {
        let calls = [decide(json!({ "verdict": "approve" }))];
        match parse_decision(&calls) {
            JudgeOutcome::Approve { reason } => assert_eq!(reason, DEFAULT_REASON),
            other => panic!("expected Approve, got {other:?}"),
        }
    }

    #[test]
    fn non_string_reason_defaults() {
        let calls = [decide(json!({ "verdict": "reject", "reason": 42 }))];
        match parse_decision(&calls) {
            JudgeOutcome::Reject { reason } => assert_eq!(reason, DEFAULT_REASON),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn unknown_verdict_is_indeterminate() {
        let calls = [decide(json!({ "verdict": "maybe", "reason": "x" }))];
        assert!(matches!(
            parse_decision(&calls),
            JudgeOutcome::Indeterminate { .. }
        ));
    }

    #[test]
    fn missing_verdict_is_indeterminate() {
        let calls = [decide(json!({ "reason": "x" }))];
        assert!(matches!(
            parse_decision(&calls),
            JudgeOutcome::Indeterminate { .. }
        ));
    }

    #[test]
    fn no_decide_call_is_indeterminate() {
        assert!(matches!(
            parse_decision(&[]),
            JudgeOutcome::Indeterminate { .. }
        ));
    }
}
