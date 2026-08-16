use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use frances_shell::{
    QuietReason, ReadEvent, RunOpts, RunOutcome, Shell, ShellOptions as LocalShellOptions, WaitOpts,
};
use frances_worker_protocol::{
    Capability, Content, ErrorCode, Feed, FeedSender, FsMetadata, Hello, PROTOCOL_VERSION,
    ProtocolError, Request, RequestKind, Response, ResponseError, ResponseKind, ShellCommand,
    ShellEvent, ShellEventKind, ShellId, ShellOperationId, ShellOptions, ShellQuietReason,
    ShellWait, multiplex,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::mpsc;

struct ServerState {
    next_shell: AtomicU64,
    shells: Mutex<HashMap<ShellId, tokio::task::AbortHandle>>,
}

impl ServerState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            next_shell: AtomicU64::new(1),
            shells: Mutex::new(HashMap::new()),
        })
    }

    fn abort_all(&self) {
        for (_, shell) in self.shells.lock().expect("shell registry poisoned").drain() {
            shell.abort();
        }
    }
}

pub async fn serve<S>(stream: S) -> Result<(), ProtocolError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    let (mut reader, writer) = multiplex(reader, writer);
    let state = ServerState::new();
    let result = loop {
        let request = match reader.receive::<Request>().await {
            Ok(Some(request)) => request,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        };
        let id = request.id;
        let shutdown = matches!(request.kind, RequestKind::Shutdown);
        let result = if request.version != PROTOCOL_VERSION {
            Err(ResponseError::new(
                ErrorCode::UnsupportedVersion,
                format!(
                    "protocol version {}, expected {PROTOCOL_VERSION}",
                    request.version
                ),
            ))
        } else {
            handle(request.kind, &state).await
        };
        if let Err(error) = writer
            .send(Response {
                version: PROTOCOL_VERSION,
                id,
                result,
            })
            .await
        {
            break Err(error);
        }
        if shutdown {
            break Ok(());
        }
    };
    state.abort_all();
    result
}

async fn handle(
    kind: RequestKind,
    state: &Arc<ServerState>,
) -> Result<ResponseKind, ResponseError> {
    match kind {
        RequestKind::Hello => Ok(ResponseKind::Hello(Hello {
            version: PROTOCOL_VERSION,
            capabilities: vec![Capability::Filesystem, Capability::Shell],
        })),
        RequestKind::FsRead { path } => {
            let file = tokio::fs::File::open(&path)
                .await
                .map_err(|error| io_error(&path, error))?;
            Ok(ResponseKind::Content(Content::from_async_read(file)))
        }
        RequestKind::FsWrite { path, content } => {
            let mut file = tokio::fs::File::create(&path)
                .await
                .map_err(|error| io_error(&path, error))?;
            content
                .copy_to(&mut file)
                .await
                .map_err(|error| io_error(&path, error))?;
            Ok(ResponseKind::Unit)
        }
        RequestKind::FsMetadata { path } => {
            let metadata = tokio::fs::metadata(&path)
                .await
                .map_err(|error| io_error(&path, error))?;
            let modified = metadata
                .modified()
                .map_err(|error| io_error(&path, error))?;
            let duration = modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|error| {
                    ResponseError::new(
                        ErrorCode::Io,
                        format!("{}: mtime before Unix epoch: {error}", path.display()),
                    )
                })?;
            let mtime_ns = i64::try_from(duration.as_nanos()).map_err(|error| {
                ResponseError::new(
                    ErrorCode::Io,
                    format!("{}: mtime overflow: {error}", path.display()),
                )
            })?;
            Ok(ResponseKind::Metadata(FsMetadata {
                mtime_ns,
                size: metadata.len(),
                is_dir: metadata.is_dir(),
            }))
        }
        RequestKind::FsCreateDirAll { path } => {
            tokio::fs::create_dir_all(&path)
                .await
                .map_err(|error| io_error(&path, error))?;
            Ok(ResponseKind::Unit)
        }
        RequestKind::FsCanonicalize { path } => tokio::fs::canonicalize(&path)
            .await
            .map(ResponseKind::Path)
            .map_err(|error| io_error(&path, error)),
        RequestKind::ShellOpen { options, commands } => open_shell(state, options, commands).await,
        RequestKind::Shutdown => Ok(ResponseKind::Unit),
    }
}

async fn open_shell(
    state: &Arc<ServerState>,
    options: ShellOptions,
    commands: Feed<ShellCommand>,
) -> Result<ResponseKind, ResponseError> {
    let shell = Shell::spawn(LocalShellOptions {
        cwd: options.cwd,
        env: options
            .env
            .into_iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect(),
        init_script: options.init_script,
    })
    .await
    .map_err(|error| ResponseError::new(ErrorCode::Io, error.to_string()))?;
    let shell_id = state.next_shell.fetch_add(1, Ordering::Relaxed);
    let (events, event_feed) = Feed::channel();
    let state_for_task = state.clone();
    let task = tokio::spawn(async move {
        run_shell(shell, commands, events).await;
        state_for_task
            .shells
            .lock()
            .expect("shell registry poisoned")
            .remove(&shell_id);
    });
    state
        .shells
        .lock()
        .expect("shell registry poisoned")
        .insert(shell_id, task.abort_handle());
    Ok(ResponseKind::ShellOpened {
        shell: shell_id,
        events: event_feed,
    })
}

async fn run_shell(
    mut shell: Shell,
    mut commands: Feed<ShellCommand>,
    events: FeedSender<ShellEvent>,
) {
    while let Ok(Some(command)) = commands.next().await {
        let keep_running = match command {
            ShellCommand::Run {
                operation,
                script,
                stdin,
                persist,
                wait,
            } => {
                let stdin = match read_optional_content(stdin).await {
                    Ok(stdin) => stdin,
                    Err(error) => {
                        send_shell_error(&events, operation, error).await;
                        continue;
                    }
                };
                observe_shell(
                    &mut shell,
                    operation,
                    &events,
                    ShellAction::Run {
                        script,
                        options: RunOpts { stdin, persist },
                        wait: wait_options(wait),
                    },
                )
                .await;
                true
            }
            ShellCommand::KeepWaiting { operation, wait } => {
                observe_shell(
                    &mut shell,
                    operation,
                    &events,
                    ShellAction::Wait(wait_options(wait)),
                )
                .await;
                true
            }
            ShellCommand::Kill { operation } => {
                match shell.kill_running().await {
                    Ok(()) => send_shell_event(&events, operation, ShellEventKind::Ack).await,
                    Err(error) => send_shell_error(&events, operation, error).await,
                }
                true
            }
            ShellCommand::SetVar {
                operation,
                name,
                value,
            } => {
                set_var(&mut shell, &events, operation, name, value).await;
                true
            }
            ShellCommand::GetVar { operation, name } => {
                get_var(&mut shell, &events, operation, name).await;
                true
            }
            ShellCommand::Close => false,
        };
        if !keep_running {
            break;
        }
    }
}

enum ShellAction {
    Run {
        script: String,
        options: RunOpts,
        wait: WaitOpts,
    },
    Wait(WaitOpts),
}

async fn observe_shell(
    shell: &mut Shell,
    operation: ShellOperationId,
    events: &FeedSender<ShellEvent>,
    action: ShellAction,
) {
    let (output, receiver) = mpsc::unbounded_channel();
    shell.set_output_sink(Some(output));
    let forward = tokio::spawn(forward_output(receiver, operation, events.clone()));
    let result = match action {
        ShellAction::Run {
            script,
            options,
            wait,
        } => shell.run_with_opts(&script, options, wait).await,
        ShellAction::Wait(wait) => shell.keep_waiting(wait).await,
    };
    shell.set_output_sink(None);
    match result {
        Ok(_) => {
            let _ = forward.await;
        }
        Err(error) => {
            forward.abort();
            send_shell_error(events, operation, error).await;
        }
    }
}

async fn forward_output(
    mut receiver: mpsc::UnboundedReceiver<ReadEvent>,
    operation: ShellOperationId,
    events: FeedSender<ShellEvent>,
) {
    while let Some(event) = receiver.recv().await {
        let (kind, terminal) = match event {
            ReadEvent::Output(bytes) => (
                ShellEventKind::Output {
                    content: Content::from_bytes(bytes),
                },
                false,
            ),
            ReadEvent::Quiet { reason } => (
                ShellEventKind::Quiet {
                    reason: quiet_reason(reason),
                },
                true,
            ),
            ReadEvent::Done { exit_code } => (ShellEventKind::Done { exit_code }, true),
            ReadEvent::Dead => (ShellEventKind::Dead, true),
        };
        if events.send(ShellEvent { operation, kind }).await.is_err() || terminal {
            return;
        }
    }
}

async fn set_var(
    shell: &mut Shell,
    events: &FeedSender<ShellEvent>,
    operation: ShellOperationId,
    name: String,
    value: Content,
) {
    if let Err(error) = validate_bash_name(&name) {
        return send_shell_error(events, operation, error).await;
    }
    if name == "FRANCES_ROOT" {
        return send_shell_error(events, operation, "FRANCES_ROOT is reserved").await;
    }
    let value = match read_content(value).await {
        Ok(value) => value,
        Err(error) => return send_shell_error(events, operation, error).await,
    };
    let result = shell
        .run_with_opts(
            &format!("export {name}=$(cat)"),
            RunOpts {
                stdin: Some(value),
                persist: vec![name],
            },
            WaitOpts::default(),
        )
        .await;
    match result {
        Ok(RunOutcome::Done { exit_code: 0, .. }) => {
            send_shell_event(events, operation, ShellEventKind::Ack).await
        }
        Ok(outcome) => {
            send_shell_error(events, operation, format!("set variable: {outcome:?}")).await
        }
        Err(error) => send_shell_error(events, operation, error).await,
    }
}

async fn get_var(
    shell: &mut Shell,
    events: &FeedSender<ShellEvent>,
    operation: ShellOperationId,
    name: String,
) {
    if let Err(error) = validate_bash_name(&name) {
        return send_shell_error(events, operation, error).await;
    }
    let result = shell
        .run(
            &format!("( set -u; printf '%s' \"${name}\" )"),
            WaitOpts::default(),
        )
        .await;
    match result {
        Ok(RunOutcome::Done {
            exit_code: 0,
            output,
        }) => {
            send_shell_event(
                events,
                operation,
                ShellEventKind::Value {
                    content: Content::from_bytes(output.into_bytes()),
                },
            )
            .await
        }
        Ok(outcome) => {
            send_shell_error(events, operation, format!("get variable: {outcome:?}")).await
        }
        Err(error) => send_shell_error(events, operation, error).await,
    }
}

fn validate_bash_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("variable name cannot be empty".into());
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || chars.any(|character| !(character.is_ascii_alphanumeric() || character == '_'))
    {
        return Err(format!("invalid bash variable name: {name}"));
    }
    Ok(())
}

async fn read_optional_content(content: Option<Content>) -> io::Result<Option<Vec<u8>>> {
    match content {
        Some(content) => read_content(content).await.map(Some),
        None => Ok(None),
    }
}

async fn read_content(content: Content) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    content
        .into_async_read()
        .await?
        .read_to_end(&mut bytes)
        .await?;
    Ok(bytes)
}

fn wait_options(wait: ShellWait) -> WaitOpts {
    WaitOpts {
        quiet: wait.quiet_ms.map(Duration::from_millis),
        max: wait.max_ms.map(Duration::from_millis),
    }
}

fn quiet_reason(reason: QuietReason) -> ShellQuietReason {
    match reason {
        QuietReason::NoOutput => ShellQuietReason::NoOutput,
        QuietReason::MaxElapsed => ShellQuietReason::MaxElapsed,
    }
}

async fn send_shell_event(
    events: &FeedSender<ShellEvent>,
    operation: ShellOperationId,
    kind: ShellEventKind,
) {
    let _ = events.send(ShellEvent { operation, kind }).await;
}

async fn send_shell_error(
    events: &FeedSender<ShellEvent>,
    operation: ShellOperationId,
    error: impl std::fmt::Display,
) {
    send_shell_event(
        events,
        operation,
        ShellEventKind::Error {
            message: error.to_string(),
        },
    )
    .await;
}

fn io_error(path: &std::path::Path, error: io::Error) -> ResponseError {
    ResponseError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
}
