//! LLM-backed auto-approver for permission gates flagged with
//! `allow_auto = true`.
//!
//! When a workflow opts a gate into auto, the daemon's emit loop
//! calls [`judge`] before forwarding the request to the TUI. The
//! judge walks the model-intent fallback `["auto", "referee", "cheap"]`
//! and gives the chosen model two tools — `approve` and `reject`,
//! each taking a single `reason` string. The decision is the *tool
//! the model picked*; the reason is informational.
//!
//! On `Approve` the daemon resolves the permission's oneshot
//! directly and the TUI never sees the prompt. On `Reject` or
//! `Indeterminate` the daemon falls through to the user — the
//! judge is an opt-in fast path, not an authority on denial.
//!
//! The judge does not see history. It does not see the structured
//! `tool_call`. It sees only the workflow-composed prompt, since
//! that prompt is already the canonical rendering of "what's being
//! requested". Feeding the structured call in too would
//! double-render.

use std::sync::Arc;
use std::sync::LazyLock;

use frances_llm::CompleteRequest;
use frances_models_llm::wire::{HistoryInput, ToolCall, ToolChoice, ToolDef, ToolFunction};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::events::PermissionRequest;

use super::ServerState;

/// Model-intent fallback walked by the judge. Resolved through the
/// chat manager's standard intent lookup; a missing intent falls
/// through to the next one and ultimately to `models::default`.
const INTENTS: &[&str] = &["auto", "referee", "cheap"];

const SYSTEM_PROMPT: &str = include_str!("auto_judge/system_prompt.md");

static APPROVE_TOOL: LazyLock<ToolDef> = LazyLock::new(|| {
    ToolDef::Function(ToolFunction {
        name: "approve".into(),
        description: include_str!("auto_judge/approve_desc.md").into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "One-sentence justification.",
                },
            },
            "required": ["reason"],
        }),
    })
});

static REJECT_TOOL: LazyLock<ToolDef> = LazyLock::new(|| {
    ToolDef::Function(ToolFunction {
        name: "reject".into(),
        description: include_str!("auto_judge/reject_desc.md").into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "One-sentence justification.",
                },
            },
            "required": ["reason"],
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

/// One round-trip outcome of asking the judge. Distinct from
/// `JudgeOutcome` because `Malformed` triggers a retry one level up.
#[derive(Debug)]
enum ParseResult {
    Approve { reason: String },
    Reject { reason: String },
    Malformed { detail: String },
}

const DEFAULT_REASON: &str = "(no reason given)";

/// Ask the configured judge model whether to auto-approve `request`.
/// One retry on malformed output before giving up with `Indeterminate`.
pub(crate) async fn judge(state: &Arc<ServerState>, request: &PermissionRequest) -> JudgeOutcome {
    let env = state
        .last_context
        .lock()
        .as_ref()
        .map(|ctx| ctx.process.env.clone())
        .unwrap_or_default();

    let session_id = format!("auto-judge:{}", request.id);
    let tools = [APPROVE_TOOL.clone(), REJECT_TOOL.clone()];

    let inputs: Vec<HistoryInput<'_>> = vec![
        HistoryInput::System {
            text: SYSTEM_PROMPT,
        },
        HistoryInput::User {
            text: request.prompt.as_str(),
        },
    ];

    let first = match run_round(state, &session_id, &env, &inputs, &tools).await {
        RoundOutcome::Parsed(parse) => parse,
        RoundOutcome::Errored(reason) => {
            warn!(%reason, id = %request.id, "auto-judge: chat.complete failed");
            return JudgeOutcome::Indeterminate { reason };
        }
    };

    match first {
        ParseResult::Approve { reason } => return JudgeOutcome::Approve { reason },
        ParseResult::Reject { reason } => return JudgeOutcome::Reject { reason },
        ParseResult::Malformed { detail } => {
            warn!(id = %request.id, %detail,
                "auto-judge: malformed response; retrying once");
        }
    }

    // One retry. Append a scold and try again; same model + tools.
    let scold = include_str!("auto_judge/scold.md");
    let mut retry_inputs = inputs.clone();
    retry_inputs.push(HistoryInput::User { text: scold });

    let second = match run_round(state, &session_id, &env, &retry_inputs, &tools).await {
        RoundOutcome::Parsed(parse) => parse,
        RoundOutcome::Errored(reason) => {
            warn!(%reason, id = %request.id, "auto-judge: retry chat.complete failed");
            return JudgeOutcome::Indeterminate {
                reason: format!("retry chat.complete failed: {reason}"),
            };
        }
    };

    match second {
        ParseResult::Approve { reason } => JudgeOutcome::Approve { reason },
        ParseResult::Reject { reason } => JudgeOutcome::Reject { reason },
        ParseResult::Malformed { detail } => JudgeOutcome::Indeterminate {
            reason: format!("judge produced no usable decision after one retry: {detail}"),
        },
    }
}

/// One round-trip wrapped so both the initial call and the retry share
/// the same cap-and-parse logic. `Errored` carries the already-formatted
/// error string; `Parsed` carries the parser verdict over whatever tool
/// calls the (cap-truncated) outcome contains.
enum RoundOutcome {
    Parsed(ParseResult),
    Errored(String),
}

async fn run_round(
    state: &Arc<ServerState>,
    session_id: &str,
    env: &std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
    inputs: &[HistoryInput<'_>],
    tools: &[ToolDef],
) -> RoundOutcome {
    let req = CompleteRequest {
        intents: INTENTS,
        session_id,
        env,
        history: &[],
        new_inputs: inputs,
        tools,
        tool_choice: Some(&ToolChoice::Required),
        cancel: CancellationToken::new(),
        // Cap at 1 — the judge prompt asks for exactly one tool call.
        // A misbehaving model that emits more gets truncated to the
        // first; we take it and proceed rather than discarding to ask
        // the user (cheaper, and the first answer is committed by the
        // model anyway). The 0-call case still falls through to
        // `Malformed("no tool calls")` → retry → Indeterminate, which
        // is the genuine "no decision" signal worth preserving.
        max_tool_calls: Some(1),
    };

    match state.chat.complete(req).await {
        Ok(outcome) => RoundOutcome::Parsed(parse_calls(&outcome.tool_calls)),
        Err(error) => RoundOutcome::Errored(format!("chat.complete failed: {error}")),
    }
}

/// Pure parser over the tool calls the judge produced. Decision is the
/// chosen tool name; reason is `arguments.reason` if present and a
/// string, otherwise a default.
fn parse_calls(calls: &[ToolCall]) -> ParseResult {
    match calls {
        [call] => {
            let reason = call
                .arguments
                .get("reason")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| DEFAULT_REASON.to_owned());
            match call.name.as_str() {
                "approve" => ParseResult::Approve { reason },
                "reject" => ParseResult::Reject { reason },
                other => ParseResult::Malformed {
                    detail: format!("unexpected tool name `{other}`"),
                },
            }
        }
        [] => ParseResult::Malformed {
            detail: "no tool calls".into(),
        },
        many => ParseResult::Malformed {
            detail: format!("{} tool calls (expected 1)", many.len()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn parses_approve() {
        let calls = [call("approve", json!({ "reason": "looks fine" }))];
        match parse_calls(&calls) {
            ParseResult::Approve { reason } => assert_eq!(reason, "looks fine"),
            other => panic!("expected Approve, got {other:?}"),
        }
    }

    #[test]
    fn parses_reject() {
        let calls = [call("reject", json!({ "reason": "rm -rf is sketchy" }))];
        match parse_calls(&calls) {
            ParseResult::Reject { reason } => assert_eq!(reason, "rm -rf is sketchy"),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn missing_reason_defaults() {
        let calls = [call("approve", json!({}))];
        match parse_calls(&calls) {
            ParseResult::Approve { reason } => assert_eq!(reason, DEFAULT_REASON),
            other => panic!("expected Approve, got {other:?}"),
        }
    }

    #[test]
    fn non_string_reason_defaults() {
        let calls = [call("reject", json!({ "reason": 42 }))];
        match parse_calls(&calls) {
            ParseResult::Reject { reason } => assert_eq!(reason, DEFAULT_REASON),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn unknown_tool_is_malformed() {
        let calls = [call("decide", json!({ "reason": "x" }))];
        assert!(matches!(parse_calls(&calls), ParseResult::Malformed { .. }));
    }

    #[test]
    fn zero_calls_is_malformed() {
        assert!(matches!(parse_calls(&[]), ParseResult::Malformed { .. }));
    }

    #[test]
    fn two_calls_is_malformed() {
        let calls = [
            call("approve", json!({ "reason": "a" })),
            call("reject", json!({ "reason": "b" })),
        ];
        assert!(matches!(parse_calls(&calls), ParseResult::Malformed { .. }));
    }
}
