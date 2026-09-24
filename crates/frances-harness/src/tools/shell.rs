use super::{ToolError, Tools, string};
use crate::{
    HarnessDeps, HarnessShell, HarnessShellHandle, Output, Outputs, PermissionRequest,
    PermissionResponse,
};
use frances_models_llm::ToolCall;
use frances_shell::{ReadEvent, RunOpts, RunOutcome, ShellOptions, WaitOpts};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(super) struct ShellView {
    id: Uuid,
    cmd: String,
    bytes: usize,
    dropped: usize,
    teaser: String,
}
impl ShellView {
    fn snapshot(&self, state: Value) -> Value {
        json!({"cmd":self.cmd,"state":state,"bytesTotal":self.bytes,"bytesDropped":self.dropped,"teaser":self.teaser})
    }
    fn chunk(&mut self, output: &Outputs, bytes: Vec<u8>) {
        self.bytes += bytes.len();
        let text = String::from_utf8_lossy(&bytes);
        self.teaser.push_str(&text);
        if self.teaser.len() > 2048 {
            let mut start = self.teaser.len() - 2048;
            while !self.teaser.is_char_boundary(start) {
                start += 1;
            }
            self.teaser.drain(..start);
        }
        // Bound the persisted live stream, independently of the model's digest.
        if self.bytes <= 256 * 1024 {
            output.send(Output::Append {
                id: self.id,
                payload: json!({"text":text}),
            });
        } else {
            self.dropped += bytes.len();
        }
        output.send(Output::Snapshot {
            id: self.id,
            kind: "shell".into(),
            snapshot: self.snapshot(json!({"type":"running"})),
        });
    }
}

#[derive(Default, Deserialize)]
struct Args {
    quiet: Option<f64>,
    max: Option<f64>,
    head: Option<usize>,
    tail: Option<usize>,
    stdin: Option<String>,
    #[serde(default)]
    persist: Vec<String>,
}
impl Args {
    fn wait(&self) -> Result<WaitOpts, ToolError> {
        fn duration(value: f64) -> Result<Duration, ToolError> {
            Duration::try_from_secs_f64(value)
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))
        }
        Ok(WaitOpts {
            quiet: Some(duration(self.quiet.unwrap_or(10.0))?),
            max: Some(duration(self.max.unwrap_or(120.0))?),
        })
    }
}

impl<D: HarnessDeps> Tools<D> {
    pub(super) async fn ensure_shell(&mut self) -> Result<(), ToolError> {
        if self.shell.is_none() {
            self.shell = Some(
                self.deps
                    .shell()
                    .spawn(ShellOptions {
                        cwd: self.deps.current_cwd(),
                        env: self
                            .deps
                            .current_env()
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                        init_script: None,
                    })
                    .await?,
            );
        }
        Ok(())
    }

    pub(super) async fn shell_call(
        &mut self,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Result<String, ToolError> {
        let args: Args = serde_json::from_value(call.arguments.clone())?;
        let wait = args.wait()?;
        if call.name == "shell_run" {
            let cmd = string(&call.arguments, "cmd")?;
            if self.shell_view.is_some() {
                return Err(ToolError::InvalidArguments(
                    "shell is busy; call shell_wait or shell_kill".into(),
                ));
            }
            let (reply, response) = oneshot::channel();
            self.output.send(Output::Permission(PermissionRequest {
                prompt: format!("Allow Frances to run this bash command?\n\n```bash\n{cmd}\n```"),
                tool_call: Some(call.clone()),
                allow_auto: true,
                reply,
            }));
            let answer = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ToolError::Interrupted),
                answer = response => answer.map_err(|_| ToolError::Interrupted)?,
            };
            if let PermissionResponse::No { details } = answer {
                return Err(ToolError::Denied(details.unwrap_or_default()));
            }
            if cancel.is_cancelled() {
                return Err(ToolError::Interrupted);
            }
            self.ensure_shell().await?;
            let snapshot = json!({"cmd":cmd,"state":{"type":"running"},"bytesTotal":0,"bytesDropped":0,"teaser":""});
            let id = self.output.open("shell", snapshot);
            self.shell_view = Some(ShellView {
                id,
                cmd: cmd.into(),
                bytes: 0,
                dropped: 0,
                teaser: String::new(),
            });
        } else if self.shell_view.is_none() {
            return Err(ToolError::NoShell);
        }
        if call.name == "shell_kill" {
            self.stop_shell().await;
            return Ok("Shell command killed.".into());
        }
        let shell = self.shell.as_mut().ok_or(ToolError::NoShell)?;
        let view = self.shell_view.as_mut().ok_or(ToolError::NoShell)?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        shell.set_output_sink(Some(tx));
        let operation = async {
            if call.name == "shell_run" {
                shell
                    .run_with_opts(
                        string(&call.arguments, "cmd")?,
                        RunOpts {
                            stdin: args.stdin.clone().map(String::into_bytes),
                            persist: args.persist.clone(),
                        },
                        wait,
                    )
                    .await
                    .map_err(ToolError::from)
            } else {
                shell.keep_waiting(wait).await.map_err(ToolError::from)
            }
        };
        let result = {
            tokio::pin!(operation);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break Err(ToolError::Interrupted),
                    Some(event) = rx.recv() => if let ReadEvent::Output(bytes) = event { view.chunk(&self.output, bytes); },
                    result = &mut operation => break result,
                }
            }
        };
        while let Ok(event) = rx.try_recv() {
            if let ReadEvent::Output(bytes) = event {
                view.chunk(&self.output, bytes);
            }
        }
        match result {
            Ok(outcome) => {
                let (status, text, state) = match outcome {
                    RunOutcome::Done { exit_code, output } => (
                        format!("Exit code: {exit_code}"),
                        output,
                        Some(if exit_code == 0 {
                            json!({"type":"success"})
                        } else {
                            json!({"type":"exit","code":exit_code})
                        }),
                    ),
                    RunOutcome::Quiet { output, .. } => (
                        "Still running; call shell_wait or shell_kill.".into(),
                        output,
                        None,
                    ),
                    RunOutcome::Dead { output } => (
                        "Shell command terminated.".into(),
                        output,
                        Some(json!({"type":"killed"})),
                    ),
                };
                let result = format!("{status}\n{}", digest(&text, args.head, args.tail));
                if let Some(state) = state {
                    self.settle_shell(state, Some(&result));
                }
                Ok(result)
            }
            Err(error) => {
                self.stop_shell().await;
                Err(error)
            }
        }
    }

    fn settle_shell(&mut self, state: Value, result: Option<&str>) {
        if let Some(view) = self.shell_view.take() {
            if view.dropped > 0 {
                self.output.send(Output::Append {
                    id: view.id,
                    payload: json!({"dropped":view.dropped}),
                });
            }
            self.output.send(Output::Settle {
                id: view.id,
                snapshot: view.snapshot(state),
                artifacts: result
                    .map(|s| vec![("llm_digest".into(), json!(s))])
                    .unwrap_or_default(),
            });
        }
    }
    pub fn shell_running(&self) -> bool {
        self.shell_view.is_some()
    }

    pub async fn stop_shell(&mut self) {
        if self.shell_view.is_some() {
            if let Some(shell) = self.shell.as_mut()
                && let Err(error) = shell.kill_running().await
            {
                tracing::warn!(%error, "kill shell command failed");
            }
            // Dropping the handle also closes the worker resource, including a
            // pending observation. A cancelled run must never poison the next one.
            self.shell = None;
            self.settle_shell(json!({"type":"killed"}), None);
        }
    }
}

fn digest(text: &str, head: Option<usize>, tail: Option<usize>) -> String {
    let lines: Vec<_> = text.lines().collect();
    let h = head.unwrap_or(0).min(lines.len());
    let t = tail
        .unwrap_or(if head.is_none() { 200 } else { 0 })
        .min(lines.len());
    if h.saturating_add(t) >= lines.len() {
        return text.into();
    }
    format!(
        "{}\n… {} lines omitted …\n{}",
        lines[..h].join("\n"),
        lines.len() - h - t,
        lines[lines.len() - t..].join("\n")
    )
}
