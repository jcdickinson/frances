//! `shell_run`, `shell_wait`, `shell_kill` — interact with the
//! per-session [`Shell`].

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use frances_shell::{QuietReason, RunOutcome, Shell, ShellError, ShellOptions, WaitOpts};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::Result;
use crate::llm::{ToolCall, ToolDef, ToolFunction};

use super::{Tool, ToolContext, ToolOutcome};

const SHELL_RUN_DESC: &str = include_str!("desc/shell_run.md");
const SHELL_WAIT_DESC: &str = include_str!("desc/shell_wait.md");
const SHELL_KILL_DESC: &str = include_str!("desc/shell_kill.md");

#[derive(Debug, Error)]
pub enum ShellToolError {
    #[error("unknown shell tool: {0}")]
    UnknownTool(String),
    #[error("parse {tool} args: {source}")]
    ParseArgs {
        tool: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("spawn shell: {0}")]
    Spawn(#[source] ShellError),
    #[error("shell run: {0}")]
    Run(#[source] ShellError),
    #[error("no active shell — call shell_run first")]
    NoActiveShell,
    #[error("shell_wait: {0}")]
    Wait(#[source] ShellError),
    #[error("shell_kill: {0}")]
    Kill(#[source] ShellError),
    #[error("drain after kill: {0}")]
    DrainAfterKill(#[source] ShellError),
}

pub struct ShellTools;

#[async_trait]
impl Tool for ShellTools {
    async fn definitions(&self) -> Result<Vec<ToolDef>> {
        Ok(vec![
            ToolDef::Function(ToolFunction {
                name: "shell_run".to_string(),
                description: SHELL_RUN_DESC.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "cmd": { "type": "string" },
                        "quiet_ms": { "type": "integer", "minimum": 0 },
                        "max_ms": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["cmd"]
                }),
            }),
            ToolDef::Function(ToolFunction {
                name: "shell_wait".to_string(),
                description: SHELL_WAIT_DESC.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "quiet_ms": { "type": "integer", "minimum": 0 },
                        "max_ms": { "type": "integer", "minimum": 0 }
                    }
                }),
            }),
            ToolDef::Function(ToolFunction {
                name: "shell_kill".to_string(),
                description: SHELL_KILL_DESC.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {}
                }),
            }),
        ])
    }

    async fn dispatch(&self, call: &ToolCall, ctx: &ToolContext<'_>) -> ToolOutcome {
        let result = match call.name.as_str() {
            "shell_run" => run_shell(&call.arguments, ctx.shell, ctx.cwd).await,
            "shell_wait" => run_keep_waiting(&call.arguments, ctx.shell).await,
            "shell_kill" => run_kill_running(ctx.shell).await,
            other => Err(ShellToolError::UnknownTool(other.to_string()).into()),
        };
        ToolOutcome::from_result(result)
    }
}

#[derive(serde::Deserialize)]
struct RunShellArgs {
    cmd: String,
    quiet_ms: Option<u64>,
    max_ms: Option<u64>,
}

#[derive(serde::Deserialize, Default)]
struct WaitArgs {
    quiet_ms: Option<u64>,
    max_ms: Option<u64>,
}

fn wait_opts(quiet_ms: Option<u64>, max_ms: Option<u64>) -> WaitOpts {
    WaitOpts {
        quiet: quiet_ms.map(Duration::from_millis),
        max: max_ms.map(Duration::from_millis),
    }
}

async fn run_shell(
    args: &Value,
    shell: &Mutex<Option<Shell>>,
    cwd: Option<&Path>,
) -> Result<String> {
    let args: RunShellArgs =
        serde_json::from_value(args.clone()).map_err(|source| ShellToolError::ParseArgs {
            tool: "shell_run",
            source,
        })?;
    let wait = wait_opts(args.quiet_ms, args.max_ms);

    let mut guard = shell.lock().await;
    if guard.as_ref().map(|s| !s.is_alive()).unwrap_or(true) {
        let opts = ShellOptions {
            cwd: cwd.map(Path::to_path_buf),
            ..ShellOptions::default()
        };
        *guard = Some(Shell::spawn(opts).await.map_err(ShellToolError::Spawn)?);
    }
    let shell = guard.as_mut().expect("shell ensured");
    let outcome = shell
        .run(&args.cmd, wait)
        .await
        .map_err(ShellToolError::Run)?;
    let result = format_outcome(&outcome);
    if matches!(outcome, RunOutcome::Dead { .. }) {
        *guard = None;
    }
    Ok(result)
}

async fn run_keep_waiting(args: &Value, shell: &Mutex<Option<Shell>>) -> Result<String> {
    let args: WaitArgs = if args.is_null() || matches!(args, Value::Object(o) if o.is_empty()) {
        WaitArgs::default()
    } else {
        serde_json::from_value(args.clone()).map_err(|source| ShellToolError::ParseArgs {
            tool: "shell_wait",
            source,
        })?
    };
    let wait = wait_opts(args.quiet_ms, args.max_ms);

    let mut guard = shell.lock().await;
    let shell = guard.as_mut().ok_or(ShellToolError::NoActiveShell)?;
    let outcome = match shell.keep_waiting(wait).await {
        Ok(o) => o,
        Err(ShellError::NoRunningCommand) => return Ok("[no command in flight]".to_string()),
        Err(e) => return Err(ShellToolError::Wait(e).into()),
    };
    let result = format_outcome(&outcome);
    if matches!(outcome, RunOutcome::Dead { .. }) {
        *guard = None;
    }
    Ok(result)
}

async fn run_kill_running(shell: &Mutex<Option<Shell>>) -> Result<String> {
    let mut guard = shell.lock().await;
    let Some(shell) = guard.as_mut() else {
        return Ok("[no active shell]".to_string());
    };
    if !shell.is_alive() {
        *guard = None;
        return Ok("[shell already dead]".to_string());
    }
    shell.kill_running().await.map_err(ShellToolError::Kill)?;
    // Drain so the LLM gets the final exit code in one round-trip rather
    // than having to follow up with shell_wait itself.
    let drain = WaitOpts {
        quiet: Some(Duration::from_secs(1)),
        max: Some(Duration::from_secs(5)),
    };
    let outcome = match shell.keep_waiting(drain).await {
        Ok(o) => o,
        Err(ShellError::NoRunningCommand) => return Ok("[no command was running]".to_string()),
        Err(e) => return Err(ShellToolError::DrainAfterKill(e).into()),
    };
    let result = format_outcome(&outcome);
    if matches!(outcome, RunOutcome::Dead { .. }) {
        *guard = None;
    }
    Ok(result)
}

fn format_outcome(outcome: &RunOutcome) -> String {
    match outcome {
        RunOutcome::Done { exit_code, output } => format!("[exit {exit_code}]\n{output}"),
        RunOutcome::Quiet { reason, output } => {
            let why = match reason {
                QuietReason::NoOutput => "output silent",
                QuietReason::MaxElapsed => "max wait reached",
            };
            format!("[still running — {why}]\n{output}")
        }
        RunOutcome::Dead { output } => format!("[shell died]\n{output}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_shell_returns_done_with_output() {
        let shell = Mutex::new(None);
        let out = run_shell(&json!({ "cmd": "echo hi" }), &shell, None)
            .await
            .unwrap();
        assert!(out.starts_with("[exit 0]\n"), "got: {out}");
        assert!(out.contains("hi"));

        // State persists across calls within the same shell mutex.
        run_shell(&json!({ "cmd": "X=42" }), &shell, None)
            .await
            .unwrap();
        let echoed = run_shell(&json!({ "cmd": "echo $X" }), &shell, None)
            .await
            .unwrap();
        assert!(echoed.contains("42"));
    }

    #[tokio::test]
    async fn run_shell_quiet_then_keep_waiting() {
        let shell = Mutex::new(None);
        let first = run_shell(
            &json!({ "cmd": "sleep 0.3; echo done", "quiet_ms": 80 }),
            &shell,
            None,
        )
        .await
        .unwrap();
        assert!(first.starts_with("[still running"), "got: {first}");

        let second = run_keep_waiting(&json!({}), &shell).await.unwrap();
        assert!(second.starts_with("[exit 0]\n"), "got: {second}");
        assert!(second.contains("done"));
    }

    #[tokio::test]
    async fn keep_waiting_without_active_shell_errors() {
        let shell = Mutex::new(None);
        let res = run_keep_waiting(&json!({}), &shell).await;
        let err = res.unwrap_err();
        assert!(matches!(
            err,
            crate::Error::ShellTool(ShellToolError::NoActiveShell)
        ));
    }

    #[tokio::test]
    async fn kill_running_aborts_and_drains() {
        let shell = Mutex::new(None);
        let first = run_shell(&json!({ "cmd": "sleep 60", "quiet_ms": 80 }), &shell, None)
            .await
            .unwrap();
        assert!(first.starts_with("[still running"));

        let killed = run_kill_running(&shell).await.unwrap();
        assert!(killed.starts_with("[exit "), "got: {killed}");

        // Shell is reusable.
        let after = run_shell(&json!({ "cmd": "echo alive" }), &shell, None)
            .await
            .unwrap();
        assert!(after.contains("alive"));
    }

    #[tokio::test]
    async fn run_shell_exit_marks_dead_and_respawns() {
        let shell = Mutex::new(None);
        let first = run_shell(&json!({ "cmd": "exit" }), &shell, None)
            .await
            .unwrap();
        assert!(first.starts_with("[shell died]"), "got: {first}");
        assert!(shell.lock().await.is_none(), "dead shell should be cleared");

        let second = run_shell(&json!({ "cmd": "echo back" }), &shell, None)
            .await
            .unwrap();
        assert!(second.contains("back"));
    }

    #[tokio::test]
    async fn run_shell_respects_cwd() {
        let shell = Mutex::new(None);
        let cwd = std::path::PathBuf::from("/tmp");
        let out = run_shell(&json!({ "cmd": "pwd" }), &shell, Some(&cwd))
            .await
            .unwrap();
        assert!(out.contains("/tmp"));
    }
}
