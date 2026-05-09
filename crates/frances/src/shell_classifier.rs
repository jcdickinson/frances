//! Pre-flight classifier for `shell_run` calls.
//!
//! The classifier opens a single-turn conversation with the
//! `models.shell_classify` model and exposes three tools — `read`,
//! `write`, `unsafe`. The model picks one and passes a one-sentence
//! `description` of what the command does; the chosen tool name is the
//! classification. Tool calls are not dispatched: the conversation
//! terminates the moment one arrives, with no tool-result round-trip.
//!
//! If the model finishes without any tool call, it gets one scolding
//! follow-up turn. Still no tool call → default to `Unsafe`, on the
//! principle that an unclassifiable command must always reach the user.
//!
//! The classifier never blocks execution itself; the caller decides
//! what to do with the result. Today the daemon just surfaces it as
//! an assistant message; a future pass will gate the actual run.
//!
//! Errors from the classifier path (LLM failure, missing config,
//! malformed tool args) collapse to `Unsafe` with the error in the
//! description, again because surfacing to the user is the safe
//! default.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::llm::{ChatClient, ModelRole, ToolCall, ToolDef, ToolFunction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Read,
    Write,
    Unsafe,
}

impl ShellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Unsafe => "unsafe",
        }
    }
}

/// Classifier verdict. `description` is the human-readable summary of
/// what the command does — produced by the model on the happy path,
/// or a fallback message when the classifier defaults to `Unsafe`.
#[derive(Debug, Clone)]
pub struct ShellClassification {
    pub kind: ShellKind,
    pub description: String,
}

const SYSTEM_PROMPT: &str = "You classify shell commands by side-effect risk. The user will give you a single shell command. Pick exactly one of the three tools (`read`, `write`, `unsafe`) and pass a one-sentence `description` of what the command does.

The dividing line is *durable side effects*, not output. Writing to stdout or stderr is NOT a side effect — it's just how shell commands talk to you. A redirect (`>`, `>>`, `tee`) or a flag that writes to disk is.

- `read`: no durable side effects. The command inspects state, computes something, or prints to stdout/stderr. Running it twice leaves the system in the same state. Examples: `ls`, `cat file`, `git log`, `grep -r foo .`, `wc`, `pwd`, `echo hi`, `printf '%s\\n' x` (no redirect), `find . -name foo`, `git status`, `cargo metadata`.

- `write`: creates, modifies, or deletes durable state inside the current workspace — files on disk, git history, project-local environments. Scope matters more than verb: deletion confined to the current directory tree is a `write`, not `unsafe`. Examples: `cargo build` (writes `target/`), `npm install`, `git commit`, `mkdir tmp/`, `echo x > foo.txt` (the `>` is the write), `printf x >> log` (the `>>` is the write), `sed -i …`, `rm -rf target/` or `rm foo.txt` (paths inside cwd), `git checkout -- file.rs`, running tests that touch fixtures.

- `unsafe`: destructive against state OUTSIDE the current workspace (`rm -rf /…`, `rm -rf ../…`, deletes targeting absolute paths or `~`, `dd of=/dev/…`, `truncate /var/…`), escalates privilege (`sudo`, `doas`), reaches the network in a mutating or exfiltrating way (`git push`, `scp`, `rsync` to remote, `ssh host cmd`), or imports untrusted bytes from the network (`curl … | sh`, `wget … && bash …`). Force operations (`git push --force`) are unsafe regardless of scope. **If you can't tell what the command does or where its writes/deletes land, default to `unsafe`.**

Decide on durability and scope, not on the verb. `printf` and `echo` are reads when they print and writes only when redirected. `cat foo` is a read; `cat > foo` is a write. `rm -rf build/` inside cwd is a write; `rm -rf /tmp/build` (absolute, outside cwd) is unsafe.

You may think out loud first, but you MUST end the turn by calling exactly one of the three tools. Do not call more than one.";

const SCOLD_PROMPT: &str = "You finished without calling a classification tool. Call exactly one of `read`, `write`, or `unsafe` now, with a `description` argument summarising what the command does.";

const FALLBACK_DESCRIPTION: &str =
    "classifier did not call a tool — defaulting to unsafe so the user is asked";

/// Classifies `cmd` and returns a verdict. Never fails: any internal
/// error collapses into a `ShellKind::Unsafe` result with the error
/// surfaced via `description` (and logged at `warn!`).
pub async fn classify_shell(llm: &ChatClient, cmd: &str) -> ShellClassification {
    match try_classify(llm, cmd).await {
        Ok(classification) => classification,
        Err(error) => {
            warn!(%error, "shell classifier errored — defaulting to unsafe");
            ShellClassification {
                kind: ShellKind::Unsafe,
                description: format!("classifier error: {error:#}"),
            }
        }
    }
}

async fn try_classify(llm: &ChatClient, cmd: &str) -> Result<ShellClassification> {
    let tools = tool_defs();
    let mut messages = vec![
        json!({"role": "system", "content": SYSTEM_PROMPT}),
        json!({"role": "user", "content": cmd}),
    ];

    // Two attempts: the original turn, then one scolding follow-up if
    // the model finished without a tool call. Three or more would just
    // burn tokens — at that point the model is unlikely to comply.
    for attempt in 0..2 {
        let outcome = llm
            .complete(ModelRole::ShellClassify, &messages, &tools, None)
            .await
            .context("shell classifier llm call")?;

        if let Some(call) = outcome.tool_calls.first() {
            return parse_classification(call);
        }

        if attempt == 0 {
            messages.push(assistant_text_payload(&outcome.text));
            messages.push(json!({"role": "user", "content": SCOLD_PROMPT}));
        }
    }

    Ok(ShellClassification {
        kind: ShellKind::Unsafe,
        description: FALLBACK_DESCRIPTION.to_string(),
    })
}

fn parse_classification(call: &ToolCall) -> Result<ShellClassification> {
    #[derive(Deserialize)]
    struct Args {
        description: String,
    }

    let kind = match call.name.as_str() {
        "read" => ShellKind::Read,
        "write" => ShellKind::Write,
        "unsafe" => ShellKind::Unsafe,
        other => return Err(anyhow!("unexpected classifier tool: {other}")),
    };
    let args: Args =
        serde_json::from_value(call.arguments.clone()).context("parse classifier args")?;
    Ok(ShellClassification {
        kind,
        description: args.description,
    })
}

fn assistant_text_payload(text: &str) -> Value {
    let content = if text.is_empty() {
        Value::Null
    } else {
        Value::String(text.to_string())
    };
    json!({ "role": "assistant", "content": content })
}

fn tool_defs() -> Vec<ToolDef> {
    [
        (
            "read",
            "The command only inspects state — no filesystem writes, no network mutation, no spawned daemons.",
        ),
        (
            "write",
            "The command modifies state in a contained, recoverable way (edits in the workspace, build/test, project-local install).",
        ),
        (
            "unsafe",
            "The command is destructive, escalates privilege, leaves the workspace, sends data over the network, or could harm the system.",
        ),
    ]
    .into_iter()
    .map(|(name, description)| {
        ToolDef::Function(ToolFunction {
            name: name.to_string(),
            description: description.to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "One-sentence description of what the shell command does."
                    }
                },
                "required": ["description"]
            }),
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_classification_happy_path() {
        let call = ToolCall {
            id: "x".into(),
            name: "read".into(),
            arguments: json!({"description": "list files"}),
        };
        let c = parse_classification(&call).unwrap();
        assert_eq!(c.kind, ShellKind::Read);
        assert_eq!(c.description, "list files");
    }

    #[test]
    fn parse_classification_unknown_tool_errors() {
        let call = ToolCall {
            id: "x".into(),
            name: "burninate".into(),
            arguments: json!({"description": "..."}),
        };
        let err = parse_classification(&call).unwrap_err();
        assert!(err.to_string().contains("burninate"));
    }

    #[test]
    fn parse_classification_missing_description_errors() {
        let call = ToolCall {
            id: "x".into(),
            name: "unsafe".into(),
            arguments: json!({}),
        };
        let err = parse_classification(&call).unwrap_err();
        assert!(err.to_string().contains("classifier args"));
    }

    #[test]
    fn tool_defs_expose_three_tools_with_required_description() {
        let defs = tool_defs();
        assert_eq!(defs.len(), 3);
        let names: Vec<&str> = defs
            .iter()
            .map(|d| match d {
                ToolDef::Function(f) => f.name.as_str(),
            })
            .collect();
        assert_eq!(names, vec!["read", "write", "unsafe"]);
        for d in &defs {
            let ToolDef::Function(f) = d;
            assert_eq!(f.parameters["required"], json!(["description"]));
            assert_eq!(
                f.parameters["properties"]["description"]["type"],
                json!("string")
            );
        }
    }

    #[test]
    fn assistant_text_payload_empty_yields_null_content() {
        let v = assistant_text_payload("");
        assert_eq!(v["role"], "assistant");
        assert!(v["content"].is_null());
    }

    #[test]
    fn shell_kind_str_matches_tool_name() {
        assert_eq!(ShellKind::Read.as_str(), "read");
        assert_eq!(ShellKind::Write.as_str(), "write");
        assert_eq!(ShellKind::Unsafe.as_str(), "unsafe");
    }
}
